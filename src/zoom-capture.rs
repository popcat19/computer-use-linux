// Purpose: Read fresh screenshot files with byte/header limits and cooperative checks before decoded allocation.

use crate::{screenshot::RawScreenshotCapture, zoom_processing::Control};
use std::{fs::File, io::Read, path::Path};

pub(crate) fn read(
    path: &Path,
    source: &str,
    limit: usize,
    control: &Control,
) -> Result<RawScreenshotCapture, String> {
    control.check()?;
    let mut file = File::open(path).map_err(|e| format!("zoom screenshot open failed: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("zoom screenshot source must be a regular file".into());
    }
    if metadata.len() > limit as u64 {
        return Err("zoom screenshot file exceeds encoded byte limit before allocation".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        control.check()?;
        let count = file.read(&mut chunk).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        if count > limit.saturating_sub(bytes.len()) {
            return Err("zoom screenshot file exceeds encoded byte limit".into());
        }
        bytes.try_reserve_exact(count).map_err(|e| e.to_string())?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    control.check()?;
    let (width, height) = crate::zoom::source_dimensions(&bytes)?;
    Ok(RawScreenshotCapture {
        mime_type: "image/png".into(),
        bytes,
        source: source.into(),
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zoom::tests::{patterned, png};
    #[test]
    fn bounded_reader_refuses_sparse_oversized_file_and_pixel_bomb_before_decode() {
        let path = std::env::temp_dir().join(format!(
            "cul-zoom-reader-{}-{}",
            std::process::id(),
            getrandom::u64().unwrap()
        ));
        File::create(&path).unwrap().set_len(1025).unwrap();
        let control = Control::new(std::time::Duration::from_secs(1));
        assert!(read(&path, "fixture", 1024, &control)
            .unwrap_err()
            .contains("before allocation"));
        let mut bytes = png(&patterned(1, 1));
        bytes[16..20].copy_from_slice(&16384u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&16384u32.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(read(&path, "fixture", 1024, &control)
            .unwrap_err()
            .contains("before decode allocation"));
        std::fs::write(&path, png(&patterned(8, 6))).unwrap();
        let raw = read(&path, "fixture", 1024, &control).unwrap();
        assert_eq!((raw.width, raw.height), (8, 6));
        control.cancel();
        assert!(read(&path, "fixture", 1024, &control).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
