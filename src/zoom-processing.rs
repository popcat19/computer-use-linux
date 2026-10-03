// Purpose: Bound screenshot pixel work, allocations, encoding and cooperative cancellation off the async runtime.

use crate::zoom::{Rect, MAX_IMAGE_BYTES};
use image::{DynamicImage, GenericImageView};
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub(crate) struct Control {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    budget: Arc<Mutex<(u64, usize)>>,
    lease: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}
pub(crate) struct CancelGuard(pub Control);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }
}
impl Control {
    pub(crate) fn new(duration: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + duration,
            budget: Arc::new(Mutex::new((0, 0))),
            lease: None,
        }
    }
    pub(crate) fn start(duration: Duration) -> Result<Self, String> {
        static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let lease = GATE
            .get_or_init(|| Arc::new(Semaphore::new(1)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| "zoom worker busy; retry after current processing completes")?;
        let mut control = Self::new(duration);
        control.lease = Some(Arc::new(lease));
        Ok(control)
    }
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub(crate) fn reserve_source(&self, bytes: &[u8]) -> Result<(), String> {
        self.check()?;
        let (width, height) = crate::zoom::source_dimensions(bytes)?;
        let work = u64::from(width) * u64::from(height);
        let mut budget = self
            .budget
            .lock()
            .map_err(|_| "zoom work budget unavailable")?;
        if budget.0 + work > 64 * 1024 * 1024 || budget.1 + bytes.len() > 32 * 1024 * 1024 {
            return Err(
                "zoom cumulative source budget exceeds 64 Mi pixels or 32 MiB encoded bytes".into(),
            );
        }
        budget.0 += work;
        budget.1 += bytes.len();
        Ok(())
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err("zoom processing cancelled".into())
        } else if Instant::now() >= self.deadline {
            self.cancel();
            Err("zoom processing deadline exceeded".into())
        } else {
            Ok(())
        }
    }
    pub(crate) fn limited(&self, duration: Duration) -> Self {
        Self {
            cancelled: self.cancelled.clone(),
            deadline: self.deadline.min(Instant::now() + duration),
            budget: self.budget.clone(),
            lease: self.lease.clone(),
        }
    }
    pub(crate) async fn job<T: Send + 'static>(
        &self,
        work: impl FnOnce(Control) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        static WORKER: OnceLock<Arc<Semaphore>> = OnceLock::new();
        self.check()?;
        let until = tokio::time::Instant::from_std(self.deadline);
        let permit = tokio::time::timeout_at(
            until,
            WORKER
                .get_or_init(|| Arc::new(Semaphore::new(1)))
                .clone()
                .acquire_owned(),
        )
        .await
        .map_err(|_| {
            self.cancel();
            "zoom worker queue deadline exceeded"
        })?
        .map_err(|_| "zoom worker unavailable")?;
        self.check()?;
        let control = self.clone();
        let job = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            control.check()?;
            work(control)
        });
        tokio::time::timeout_at(until, job)
            .await
            .map_err(|_| {
                self.cancel();
                "zoom processing deadline exceeded"
            })?
            .map_err(|e| format!("zoom worker failed: {e}"))?
    }
}

struct CappedPng {
    bytes: Vec<u8>,
    cap: usize,
    control: Control,
}
impl Write for CappedPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.control.check().map_err(io::Error::other)?;
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "zoom PNG byte budget exhausted (4 MiB total)",
            ));
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let capacity = needed
                .max(self.bytes.capacity().saturating_mul(2))
                .min(self.cap);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.control.check().map_err(io::Error::other)
    }
}
pub(crate) fn enlarge_png(
    image: &DynamicImage,
    rect: &Rect,
    factor: u32,
    cap: usize,
    control: &Control,
) -> Result<Vec<u8>, String> {
    control.check()?;
    if cap == 0 || cap > MAX_IMAGE_BYTES {
        return Err("zoom PNG byte budget exhausted".into());
    }
    let width = rect.width * factor;
    let height = rect.height * factor;
    let sixteen = matches!(
        image,
        image::DynamicImage::ImageLuma16(_)
            | image::DynamicImage::ImageLumaA16(_)
            | image::DynamicImage::ImageRgb16(_)
            | image::DynamicImage::ImageRgba16(_)
    );
    let mut sink = CappedPng {
        bytes: Vec::new(),
        cap,
        control: control.clone(),
    };
    {
        let mut encoder = png::Encoder::new(&mut sink, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(if sixteen {
            png::BitDepth::Sixteen
        } else {
            png::BitDepth::Eight
        });
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        {
            let mut stream = writer
                .stream_writer_with_size(4096)
                .map_err(|e| e.to_string())?;
            let bpp = if sixteen { 8 } else { 4 };
            let mut row = vec![0u8; width as usize * bpp];
            for y in 0..height {
                control.check()?;
                for x in 0..width {
                    let px = rect.x + x / factor;
                    let py = rect.y + y / factor;
                    if sixteen {
                        let pixel = match image {
                            DynamicImage::ImageLuma16(buffer) => {
                                let [v] = buffer.get_pixel(px, py).0;
                                [v, v, v, u16::MAX]
                            }
                            DynamicImage::ImageLumaA16(buffer) => {
                                let [v, a] = buffer.get_pixel(px, py).0;
                                [v, v, v, a]
                            }
                            DynamicImage::ImageRgb16(buffer) => {
                                let [r, g, b] = buffer.get_pixel(px, py).0;
                                [r, g, b, u16::MAX]
                            }
                            DynamicImage::ImageRgba16(buffer) => buffer.get_pixel(px, py).0,
                            _ => unreachable!(),
                        };
                        for (channel, value) in pixel.into_iter().enumerate() {
                            row[x as usize * 8 + channel * 2..x as usize * 8 + channel * 2 + 2]
                                .copy_from_slice(&value.to_be_bytes());
                        }
                    } else {
                        row[x as usize * 4..x as usize * 4 + 4]
                            .copy_from_slice(&image.get_pixel(px, py).0);
                    }
                }
                stream.write_all(&row).map_err(|e| e.to_string())?;
            }
            stream.finish().map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }
    control.check()?;
    Ok(sink.bytes)
}

#[cfg(test)]
pub(crate) fn test_serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn crop_image(
    image: DynamicImage,
    rect: &Rect,
    control: &Control,
) -> Result<DynamicImage, String> {
    macro_rules! crop {
        ($variant:ident,$buffer:expr) => {
            DynamicImage::$variant(crop_buffer(&$buffer, rect, control)?)
        };
    }
    control.check()?;
    Ok(match image {
        DynamicImage::ImageLuma8(buffer) => crop!(ImageLuma8, buffer),
        DynamicImage::ImageLumaA8(buffer) => crop!(ImageLumaA8, buffer),
        DynamicImage::ImageRgb8(buffer) => crop!(ImageRgb8, buffer),
        DynamicImage::ImageRgba8(buffer) => crop!(ImageRgba8, buffer),
        DynamicImage::ImageLuma16(buffer) => crop!(ImageLuma16, buffer),
        DynamicImage::ImageLumaA16(buffer) => crop!(ImageLumaA16, buffer),
        DynamicImage::ImageRgb16(buffer) => crop!(ImageRgb16, buffer),
        DynamicImage::ImageRgba16(buffer) => crop!(ImageRgba16, buffer),
        _ => return Err("unsupported screenshot pixel type".into()),
    })
}
fn crop_buffer<P: image::Pixel + 'static>(
    image: &image::ImageBuffer<P, Vec<P::Subpixel>>,
    rect: &Rect,
    control: &Control,
) -> Result<image::ImageBuffer<P, Vec<P::Subpixel>>, String> {
    control.check()?;
    let mut output = image::ImageBuffer::<P, Vec<P::Subpixel>>::new(rect.width, rect.height);
    let channels = usize::from(P::CHANNEL_COUNT);
    let stride = rect.width as usize * channels;
    for y in 0..rect.height {
        control.check()?;
        let start = ((rect.y + y) as usize * image.width() as usize + rect.x as usize) * channels;
        output.as_mut()[y as usize * stride..(y as usize + 1) * stride]
            .copy_from_slice(&image.as_raw()[start..start + stride]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoom::{
        source_dimensions,
        tests::{patterned, png},
    };
    fn header(width: u32, height: u32, depth: u8) -> Vec<u8> {
        let mut bytes = png(&patterned(1, 1));
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes[24] = depth;
        bytes
    }
    #[test]
    fn dimension_and_sixteen_bit_allocation_bombs_are_refused_before_decoder() {
        for bytes in [
            header(16384, 16384, 8),
            header(8192, 8192, 16),
            header(u32::MAX, 1, 8),
        ] {
            let error = source_dimensions(&bytes).unwrap_err();
            assert!(error.contains("before decode allocation"));
            assert!(crate::zoom::decode(&bytes)
                .unwrap_err()
                .contains("before decode allocation"));
        }
    }
    #[test]
    fn cumulative_duplicate_source_work_and_bytes_are_charged_before_allocation() {
        let control = Control::new(Duration::from_secs(1));
        let bytes = header(8192, 4096, 8);
        control.reserve_source(&bytes).unwrap();
        control.reserve_source(&bytes).unwrap();
        assert!(control
            .reserve_source(&bytes)
            .unwrap_err()
            .contains("cumulative"));
        let control = Control::new(Duration::from_secs(1));
        let mut bytes = header(1, 1, 8);
        bytes.resize(crate::zoom::MAX_SOURCE_BYTES, 0);
        control.reserve_source(&bytes).unwrap();
        control.reserve_source(&bytes).unwrap();
        assert!(control.reserve_source(&bytes).is_err());
    }
    #[test]
    fn streamed_png_sink_caps_bytes_and_preserves_sixteen_bit_samples() {
        let control = Control::new(Duration::from_secs(1));
        assert!(enlarge_png(
            &patterned(32, 32),
            &Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32
            },
            2,
            32,
            &control
        )
        .unwrap_err()
        .contains("byte budget"));
        let input = image::ImageBuffer::from_fn(2, 2, |x, y| {
            image::Rgba([420 + x as u16, 65512 - y as u16, 32769, 65535])
        });
        let image = DynamicImage::ImageRgba16(input.clone());
        let bytes = enlarge_png(
            &image,
            &Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            2,
            4096,
            &control,
        )
        .unwrap();
        let output = crate::zoom::decode(&bytes).unwrap();
        let output = output.as_rgba16().unwrap();
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(output.get_pixel(x, y), input.get_pixel(x / 2, y / 2));
            }
        }
    }
    #[tokio::test]
    async fn slow_worker_does_not_block_runtime_and_cancel_guard_stops_cooperative_work() {
        let control = Control::new(Duration::from_secs(2));
        let guard = CancelGuard(control.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let worker = control.clone();
        let task = tokio::spawn(async move {
            worker
                .job(move |control| {
                    started_tx.send(()).unwrap();
                    loop {
                        control.check()?;
                        std::thread::sleep(Duration::from_millis(2));
                    }
                })
                .await as Result<(), String>
        });
        started_rx.await.unwrap();
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(started.elapsed() < Duration::from_millis(250));
        drop(guard);
        assert!(task.await.unwrap().unwrap_err().contains("cancelled"));
    }
    #[tokio::test]
    async fn timed_out_worker_keeps_lease_until_bounded_noninterruptible_stage_finishes() {
        let _serial = test_serial().lock().await;
        let control = Control::start(Duration::from_secs(2)).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let child = control.limited(Duration::from_millis(20));
        let task = tokio::spawn(async move {
            child
                .job(move |control| {
                    tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(80));
                    control.check()
                })
                .await
        });
        rx.await.unwrap();
        assert!(Control::start(Duration::from_secs(1)).is_err());
        assert!(task.await.unwrap().is_err());
        assert!(control.check().is_err());
        drop(control);
        assert!(Control::start(Duration::from_secs(1)).is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(Control::start(Duration::from_secs(1)).is_ok());
    }
}
