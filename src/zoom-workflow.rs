// Purpose: Resolve scoped fresh or script-retained sources for bounded screenshot zoom.

use super::*;
use crate::zoom::{Association, Geometry, Output, Pixels, Retained, Source, ZoomParams};
use crate::zoom_processing::{CancelGuard, Control};

impl ComputerUseLinux {
    pub(super) async fn perform_zoom(
        &self,
        params: ZoomParams,
        retained: Option<Vec<std::result::Result<Option<Retained>, String>>>,
    ) -> CallToolResult {
        self.perform_zoom_bounded(params, retained, Duration::from_secs(45))
            .await
    }
    pub(super) async fn perform_zoom_bounded(
        &self,
        params: ZoomParams,
        retained: Option<Vec<std::result::Result<Option<Retained>, String>>>,
        duration: Duration,
    ) -> CallToolResult {
        if let Err(error) = params.validate() {
            return CallToolResult::error(vec![Content::text(error)]);
        }
        if retained.is_none() && params.sources.iter().any(|source| source.image.is_some()) {
            return CallToolResult::error(vec![Content::text(
                "prior-image references are only valid inside the same run_script",
            )]);
        }
        #[cfg(test)]
        let _test_serial = crate::zoom_processing::test_serial().lock().await;
        let control = match Control::start(duration.min(Duration::from_secs(45))) {
            Ok(control) => control,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        let _cancel = CancelGuard(control.clone());
        let mut planned_work = 0u64;
        let preflight: Vec<_> = params
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let mut errors = self.preflight_regions(source);
                for (region, error) in source.regions.iter().zip(&mut errors) {
                    if error.is_some() {
                        continue;
                    }
                    let bounds = if source.image.is_some() {
                        match retained.as_ref().and_then(|saved| saved.get(index)) {
                            Some(Ok(Some(saved))) => retained_region_bounds(saved, region),
                            Some(Err(message)) => Err(message.clone()),
                            _ => Err("invalid, stale, or foreign script image handle".into()),
                        }
                    } else if let (Some(rect), Some(reference)) = (&region.rect, &source.reference)
                    {
                        crate::zoom::map_rect(
                            rect,
                            (reference.width, reference.height),
                            (reference.coordinate_width, reference.coordinate_height),
                        )
                    } else {
                        self.zoom_association
                            .lock()
                            .map_err(|_| "zoom association cache unavailable".to_string())
                            .and_then(|cached| {
                                cached
                                    .elements
                                    .get(&region.element_index.unwrap())
                                    .cloned()
                                    .ok_or("element bounds unavailable".into())
                            })
                    };
                    match bounds {
                        Err(message) => *error = Some(message),
                        Ok(bounds) => {
                            let factor = u64::from(region.factor.unwrap_or(2));
                            let width = u64::from(bounds.width) * factor;
                            let height = u64::from(bounds.height) * factor;
                            if width > 4096 || height > 4096 {
                                *error = Some("enlarged output exceeds dimension limit".into());
                            } else if planned_work + width * height > 64 * 1024 * 1024 {
                                *error = Some(
                                    "zoom cumulative planned output work exceeds 64 Mi pixels"
                                        .into(),
                                );
                            } else {
                                planned_work += width * height;
                            }
                        }
                    }
                }
                errors
            })
            .collect();
        let mut retained: Vec<_> = retained
            .unwrap_or_else(|| (0..params.sources.len()).map(|_| Ok(None)).collect())
            .into_iter()
            .map(Some)
            .collect();
        collect_controlled(
            &params.sources,
            &preflight,
            |index, source| {
                let saved = retained
                    .get_mut(index)
                    .and_then(Option::take)
                    .unwrap_or(Ok(None));
                let control = control.limited(Duration::from_secs(12));
                Box::pin(async move {
                    control.check()?;
                    if source.image.is_some() {
                        let saved =
                            saved?.ok_or("invalid, stale, or foreign script image handle")?;
                        control
                            .job(move |control| {
                                let bytes = if let Some(encoded) = saved.encoded {
                                    use base64::Engine;
                                    base64::engine::general_purpose::STANDARD
                                        .decode(encoded)
                                        .map_err(|_| "invalid retained image encoding")?
                                } else {
                                    saved.bytes
                                };
                                control.reserve_source(&bytes)?;
                                let pixels = Retained {
                                    bytes,
                                    encoded: None,
                                    geometry: saved.geometry,
                                    association: saved.association,
                                }
                                .decode()?;
                                control.check()?;
                                Ok(pixels)
                            })
                            .await
                    } else {
                        self.fresh_zoom_source(source, &control).await
                    }
                })
            },
            control.clone(),
        )
        .await
    }

    fn preflight_regions(&self, source: &Source) -> Vec<Option<String>> {
        source
            .regions
            .iter()
            .map(|region| {
                if let Some(rect) = &region.rect {
                    if let Some(reference) = &source.reference {
                        let mapped = crate::zoom::map_rect(
                            rect,
                            (reference.width, reference.height),
                            (reference.coordinate_width, reference.coordinate_height),
                        );
                        match mapped {
                            Err(error) => return Some(error),
                            Ok(rect) => {
                                let factor = region.factor.unwrap_or(2);
                                if u64::from(rect.width) * u64::from(factor) > 4096
                                    || u64::from(rect.height) * u64::from(factor) > 4096
                                {
                                    return Some(
                                        "enlarged rectangle exceeds output dimension limit".into(),
                                    );
                                }
                            }
                        }
                    }
                    return None;
                }
                if source.image.is_some() {
                    return None;
                }
                self.preflight_element(
                    source,
                    region.element_index.unwrap(),
                    region.factor.unwrap_or(2),
                )
                .err()
            })
            .collect()
    }
    fn preflight_element(
        &self,
        source: &Source,
        index: u32,
        factor: u32,
    ) -> std::result::Result<(), String> {
        let cached = self
            .zoom_association
            .lock()
            .map_err(|_| "zoom association cache unavailable")?;
        let target = source
            .target
            .as_ref()
            .ok_or("fresh elements require an explicit cached window target")?;
        let window = cached
            .window
            .as_ref()
            .ok_or("fresh elements require get_app_state with verified window bounds")?;
        resolve_window_target(std::slice::from_ref(window), target)
            .map_err(|_| "element cache belongs to another window scope")?;
        let rect = cached
            .elements
            .get(&index)
            .ok_or("element bounds unavailable or unverified for this window scope")?;
        if !cached.identities.contains_key(&index) {
            return Err("element object identity unavailable; capture get_app_state again".into());
        }
        if u64::from(rect.width) * u64::from(factor) > 4096
            || u64::from(rect.height) * u64::from(factor) > 4096
        {
            return Err("enlarged element exceeds output dimension limit".into());
        }
        Ok(())
    }
    #[cfg(test)]
    fn preflight_zoom_source(&self, source: &Source) -> std::result::Result<(), String> {
        self.preflight_regions(source)
            .into_iter()
            .flatten()
            .next()
            .map_or(Ok(()), Err)
    }

    async fn fresh_zoom_source(
        &self,
        source: &Source,
        control: &Control,
    ) -> std::result::Result<Pixels, String> {
        control.check()?;
        #[cfg(test)]
        if let Some(fixtures) = &self.zoom_fixtures {
            let fixture = fixtures
                .lock()
                .map_err(|_| "fixture lock")?
                .pop_front()
                .ok_or("fixture exhausted")?;
            if let Some(target) = &source.target {
                let window = fixture
                    .window
                    .as_ref()
                    .ok_or("fixture target unresolved; no desktop fallback")?;
                resolve_window_target(std::slice::from_ref(window), target)
                    .map_err(|_| "fixture target unresolved; no desktop fallback")?;
            }
            let cached = self
                .zoom_association
                .lock()
                .map_err(|_| "zoom association cache unavailable")?
                .clone();
            let association = fixture
                .window
                .as_ref()
                .map(|window| {
                    crate::zoom_association::refresh(
                        &cached,
                        &fixture.nodes,
                        window,
                        fixture
                            .map
                            .portal_rect
                            .unwrap_or(fixture.map.full_capture_rect),
                        fixture.map.full_capture_rect,
                        fixture.map.capture_rect,
                        (fixture.raw.width, fixture.raw.height),
                    )
                })
                .unwrap_or_else(|| Association {
                    full_dimensions: (fixture.raw.width, fixture.raw.height),
                    ..Default::default()
                });
            return decode_fresh(
                source.clone(),
                fixture.raw,
                fixture.map.capture_rect,
                association,
                control,
            )
            .await;
        }
        let window = match source.target.as_ref() {
            Some(target) => {
                control.check()?;
                if source.raise_window.unwrap_or(false) {
                    let windows = list_windows()
                        .await
                        .map_err(|e| format!("zoom target preflight failed: {e:#}"))?;
                    let planned = resolve_window_target(&windows, target)
                        .map_err(|e| format!("zoom target preflight failed: {e:#}"))?;
                    if planned.bounds.as_ref().and_then(window_crop_rect).is_none() {
                        return Err("zoom target has unavailable bounds; refusing focus before safe capture geometry".into());
                    }
                    control.check()?;
                }
                Some(
                    self.resolve_screenshot_window(target, source.raise_window.unwrap_or(false))
                        .await
                        .map_err(|e| format!("zoom target failed: {e:#}"))?,
                )
            }
            None => None,
        };
        control.check()?;
        let raw = crate::screenshot_impl::capture_zoom_raw(control)
            .await
            .map_err(|e| format!("zoom capture failed: {e:#}"))?;
        control.check()?;
        let (crop, association) = if let Some(window) = &window {
            let map = self
                .window_coordinate_map_for_dimensions(window, raw.width, raw.height)
                .await
                .map_err(|e| format!("zoom geometry failed: {e:#}"))?;
            let cached = self
                .zoom_association
                .lock()
                .map_err(|_| "zoom association cache unavailable")?
                .clone();
            let compatible = association_for_capture(
                cached,
                window,
                (map.capture_rect.0, map.capture_rect.1),
                (raw.width, raw.height),
            );
            let association = if !compatible.identities.is_empty() {
                control.check()?;
                if let Some(pid) = window.pid {
                    match snapshot_accessibility_tree(None, Some(pid), 2000, 64).await {
                        Ok(snapshot) if snapshot.scoped && !snapshot.truncated => {
                            crate::zoom_association::refresh(
                                &compatible,
                                &snapshot.nodes,
                                window,
                                map.portal_rect.unwrap_or(map.full_capture_rect),
                                map.full_capture_rect,
                                map.capture_rect,
                                (raw.width, raw.height),
                            )
                        }
                        _ => Association {
                            window: Some(window.clone()),
                            origin: (map.capture_rect.0, map.capture_rect.1),
                            full_dimensions: (raw.width, raw.height),
                            ..Default::default()
                        },
                    }
                } else {
                    Association {
                        window: Some(window.clone()),
                        origin: (map.capture_rect.0, map.capture_rect.1),
                        full_dimensions: (raw.width, raw.height),
                        ..Default::default()
                    }
                }
            } else {
                Association {
                    window: Some(window.clone()),
                    origin: (map.capture_rect.0, map.capture_rect.1),
                    full_dimensions: (raw.width, raw.height),
                    ..Default::default()
                }
            };
            (map.capture_rect, association)
        } else {
            (
                (0, 0, raw.width, raw.height),
                Association {
                    full_dimensions: (raw.width, raw.height),
                    ..Default::default()
                },
            )
        };
        control.check()?;
        decode_fresh(source.clone(), raw, crop, association, control).await
    }
}

fn retained_region_bounds(
    saved: &Retained,
    region: &crate::zoom::Region,
) -> std::result::Result<crate::zoom::Rect, String> {
    let geometry = &saved.geometry;
    if let Some(rect) = &region.rect {
        rect.validate(geometry.width, geometry.height)?;
        Ok(rect.clone())
    } else {
        let rect = saved
            .association
            .elements
            .get(&region.element_index.unwrap())
            .ok_or("element bounds unavailable for retained image scope")?;
        crate::zoom::map_rect(
            rect,
            (geometry.coordinate_width, geometry.coordinate_height),
            (geometry.width, geometry.height),
        )
    }
}

async fn decode_fresh(
    source: Source,
    raw: RawScreenshotCapture,
    crop: (i32, i32, u32, u32),
    association: Association,
    control: &Control,
) -> std::result::Result<Pixels, String> {
    control
        .job(move |control| {
            control.reserve_source(&raw.bytes)?;
            let image = crate::zoom::decode(&raw.bytes)?;
            control.check()?;
            if image.width() != raw.width || image.height() != raw.height {
                return Err("capture dimensions disagree with decoded pixels".into());
            }
            let rect = crate::zoom::Rect {
                x: u32::try_from(crop.0).map_err(|_| "invalid crop origin")?,
                y: u32::try_from(crop.1).map_err(|_| "invalid crop origin")?,
                width: crop.2,
                height: crop.3,
            };
            rect.validate(raw.width, raw.height)?;
            let image = if rect.x == 0
                && rect.y == 0
                && rect.width == raw.width
                && rect.height == raw.height
            {
                image
            } else {
                crate::zoom_processing::crop_image(image, &rect, &control)?
            };
            control.check()?;
            fresh_pixels(&source, image, association)
        })
        .await
}

fn association_for_capture(
    cached: Association,
    window: &WindowInfo,
    origin: (i32, i32),
    dimensions: (u32, u32),
) -> Association {
    if cached.window.as_ref().is_some_and(|old| {
        old.window_id == window.window_id
            && old.backend == window.backend
            && old.pid == window.pid
            && old.app_id == window.app_id
            && old.wm_class == window.wm_class
            && old.title == window.title
            && serde_json::to_value(&old.bounds).ok() == serde_json::to_value(&window.bounds).ok()
    }) && cached.origin == origin
        && cached.full_dimensions == dimensions
    {
        cached
    } else {
        Association {
            window: Some(window.clone()),
            origin,
            full_dimensions: dimensions,
            ..Default::default()
        }
    }
}

async fn collect_controlled<'a, F>(
    sources: &'a [Source],
    preflight: &[Vec<Option<String>>],
    mut acquire: F,
    control: Control,
) -> CallToolResult
where
    F: FnMut(
        usize,
        &'a Source,
    ) -> futures_util::future::BoxFuture<'a, std::result::Result<Pixels, String>>,
{
    let mut output = Output::new();
    for (index, source) in sources.iter().enumerate() {
        let errors = &preflight[index];
        if errors.iter().all(Option::is_some) {
            for (region, error) in source.regions.iter().zip(errors) {
                output.error(index, &region.label, error.clone().unwrap());
            }
            continue;
        }
        let pixels = match control.check() {
            Err(error) => Err(error),
            Ok(()) => {
                match tokio::time::timeout(
                    control.remaining().min(Duration::from_secs(12)),
                    acquire(index, source),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        control.cancel();
                        Err("zoom source deadline exceeded".into())
                    }
                }
            }
        };
        match pixels {
            Ok(pixels) => {
                let pixels = Arc::new(pixels);
                for (region, error) in source.regions.iter().zip(errors) {
                    if let Some(error) = error {
                        output.error(index, &region.label, error.clone());
                        continue;
                    }
                    let result = match control
                        .check()
                        .and_then(|()| output.reserve_work(&pixels, region))
                    {
                        Err(error) => Err(error),
                        Ok(()) => {
                            let pixels = pixels.clone();
                            let region = region.clone();
                            let cap = crate::zoom::MAX_IMAGE_BYTES - output.bytes;
                            control
                                .job(move |control| {
                                    crate::zoom::crop_bounded(&pixels, &region, cap, &control)
                                })
                                .await
                        }
                    };
                    output.add_processed(index, &region.label, result);
                }
            }
            Err(error) => {
                for (region, preflight) in source.regions.iter().zip(errors) {
                    output.error(
                        index,
                        &region.label,
                        preflight.clone().unwrap_or_else(|| error.clone()),
                    );
                }
            }
        }
    }
    output.finish()
}

#[cfg(test)]
async fn collect_sources<'a, F>(sources: &'a [Source], acquire: F) -> CallToolResult
where
    F: FnMut(
        usize,
        &'a Source,
    ) -> futures_util::future::BoxFuture<'a, std::result::Result<Pixels, String>>,
{
    let errors: Vec<_> = sources
        .iter()
        .map(|source| vec![None; source.regions.len()])
        .collect();
    collect_controlled(
        sources,
        &errors,
        acquire,
        Control::new(Duration::from_secs(45)),
    )
    .await
}
fn fresh_pixels(
    source: &Source,
    image: image::DynamicImage,
    association: Association,
) -> std::result::Result<Pixels, String> {
    let geometry = source.reference.clone().unwrap_or(Geometry {
        width: image.width(),
        height: image.height(),
        coordinate_width: image.width(),
        coordinate_height: image.height(),
    });
    if geometry.coordinate_width != image.width() || geometry.coordinate_height != image.height() {
        return Err(
            "current source geometry differs from reference screenshot; capture a new reference"
                .into(),
        );
    }
    Ok(Pixels {
        image,
        geometry,
        association,
        retained: false,
    })
}

#[cfg(test)]
pub(super) struct Fixture {
    pub raw: RawScreenshotCapture,
    pub window: Option<WindowInfo>,
    pub map: WindowCoordinateMap,
    pub nodes: Vec<AccessibilityNode>,
}

#[cfg(test)]
#[path = "zoom-workflow-tests.rs"]
mod tests;

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use crate::zoom::tests::patterned;
    #[tokio::test]
    async fn internal_timeout_preserves_prior_crops_and_prevents_next_source_acquisition() {
        let _serial = crate::zoom_processing::test_serial().lock().await;
        let params:ZoomParams=serde_json::from_value(serde_json::json!({"sources":[
            {"image":{"$image":"fixture:0"},"regions":[{"label":"first","rect":{"x":0,"y":0,"width":1,"height":1}}]},
            {"image":{"$image":"fixture:1"},"regions":[{"label":"slow","rect":{"x":0,"y":0,"width":1,"height":1}}]},
            {"target":{"window_id":2},"reference":{"width":2,"height":2,"coordinate_width":2,"coordinate_height":2},"raise_window":true,"regions":[{"label":"next","rect":{"x":0,"y":0,"width":1,"height":1}}]}
        ]})).unwrap();
        let control = Control::new(Duration::from_secs(2));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = calls.clone();
        let work = control.clone();
        let preflight = vec![vec![None]; 3];
        let result = collect_controlled(
            &params.sources,
            &preflight,
            move |index, _| {
                log.lock().unwrap().push(index);
                let work = work.clone();
                Box::pin(async move {
                    if index == 1 {
                        return work
                            .limited(Duration::from_millis(20))
                            .job(|control| {
                                for _ in 0..100 {
                                    control.check()?;
                                    std::thread::sleep(Duration::from_millis(2));
                                }
                                Err("slow fixture unexpectedly finished".into())
                            })
                            .await;
                    }
                    Ok(Pixels {
                        image: patterned(2, 2),
                        geometry: Geometry {
                            width: 2,
                            height: 2,
                            coordinate_width: 2,
                            coordinate_height: 2,
                        },
                        association: Association::default(),
                        retained: false,
                    })
                })
            },
            control,
        )
        .await;
        assert_eq!(*calls.lock().unwrap(), vec![0, 1]);
        assert_eq!(
            result
                .content
                .iter()
                .filter(|block| block.as_image().is_some())
                .count(),
            1
        );
        assert_eq!(result.is_error, Some(true));
        let metadata = result.structured_content.unwrap();
        assert_eq!(metadata["regions"][0]["ok"], true);
        assert_eq!(metadata["regions"][2]["ok"], false);
    }
}

#[cfg(test)]
#[path = "zoom-wire-tests.rs"]
mod wire_tests;
