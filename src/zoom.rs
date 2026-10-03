// Purpose: Validate screenshot-region requests and enlarge captured pixels without synthetic detail.

use base64::{engine::general_purpose::STANDARD, Engine};
use image::{DynamicImage, GenericImageView, ImageReader, Limits};
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, io::Cursor};

pub(crate) const MAX_REGIONS: usize = 16;
pub(crate) const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_SOURCE_PIXELS: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_PIXELS: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Geometry {
    pub width: u32,
    pub height: u32,
    pub coordinate_width: u32,
    pub coordinate_height: u32,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageRef {
    #[serde(rename = "$image")]
    pub handle: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Region {
    pub label: String,
    /// Rectangle in the actual reference screenshot preview pixels, not desktop coordinates.
    pub rect: Option<Rect>,
    pub element_index: Option<u32>,
    /// Integer nearest-neighbor enlargement, default 2, range 1..8.
    #[schemars(range(min = 1, max = 8))]
    pub factor: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    /// Existing script-local {$image: ...} reference, only valid inside the same run_script.
    pub image: Option<ImageRef>,
    /// Fresh capture target. Omit for the desktop. Never falls back for unresolved windows.
    pub target: Option<crate::windowing::WindowTarget>,
    /// Required for fresh rectangle selections: dimensions from the reference screenshot.
    pub reference: Option<Geometry>,
    /// Explicitly permit raising a fresh target before capture, default false.
    pub raise_window: Option<bool>,
    #[schemars(length(min = 1, max = 16))]
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ZoomParams {
    /// 1..4 sources, at most 16 labeled regions in total.
    #[schemars(length(min = 1, max = 4))]
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Association {
    #[serde(default)]
    pub identities: BTreeMap<u32, ElementIdentity>,
    #[serde(default)]
    pub frame_object_ref: Option<String>,
    #[serde(default = "unit_scale")]
    pub capture_scale: [f64; 2],
    #[serde(default)]
    pub capture_offset: Option<[f64; 2]>,
    pub window: Option<crate::windowing::WindowInfo>,
    pub elements: BTreeMap<u32, Rect>,
    pub origin: (i32, i32),
    pub full_dimensions: (u32, u32),
}

impl Default for Association {
    fn default() -> Self {
        Self {
            window: None,
            elements: BTreeMap::new(),
            origin: (0, 0),
            full_dimensions: (0, 0),
            identities: BTreeMap::new(),
            frame_object_ref: None,
            capture_scale: unit_scale(),
            capture_offset: None,
        }
    }
}

fn unit_scale() -> [f64; 2] {
    [1.0, 1.0]
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ElementIdentity {
    pub object_ref: String,
    pub role: String,
    pub name: Option<String>,
}

pub(crate) struct Retained {
    pub encoded: Option<String>,
    pub bytes: Vec<u8>,
    pub geometry: Geometry,
    pub association: Association,
}

impl Retained {
    pub(crate) fn decode(self) -> Result<Pixels, String> {
        let bytes = if let Some(encoded) = self.encoded {
            STANDARD
                .decode(encoded)
                .map_err(|_| "invalid retained image encoding")?
        } else {
            self.bytes
        };
        let image = decode(&bytes)?;
        if image.dimensions() != (self.geometry.width, self.geometry.height) {
            return Err("retained image dimensions disagree with metadata".into());
        }
        Ok(Pixels {
            image,
            geometry: self.geometry,
            association: self.association,
            retained: true,
        })
    }
}
pub(crate) struct Pixels {
    pub image: DynamicImage,
    pub geometry: Geometry,
    pub association: Association,
    pub retained: bool,
}
impl Rect {
    pub(crate) fn validate(&self, width: u32, height: u32) -> Result<(), String> {
        if self.width == 0
            || self.height == 0
            || self.x.checked_add(self.width).is_none_or(|end| end > width)
            || self
                .y
                .checked_add(self.height)
                .is_none_or(|end| end > height)
        {
            return Err("rectangle is empty, overflowing, or outside the source image".into());
        }
        Ok(())
    }
}
impl Geometry {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if [
            self.width,
            self.height,
            self.coordinate_width,
            self.coordinate_height,
        ]
        .iter()
        .any(|n| *n == 0 || *n > 16384)
            || u64::from(self.coordinate_width) * u64::from(self.coordinate_height)
                > MAX_SOURCE_PIXELS
            || u64::from(self.width) * u64::from(self.height) > MAX_SOURCE_PIXELS
        {
            return Err("invalid or oversized reference screenshot geometry".into());
        }
        let error = (f64::from(self.width) / f64::from(self.coordinate_width)
            - f64::from(self.height) / f64::from(self.coordinate_height))
        .abs();
        if error > 1.0 / f64::from(self.coordinate_width) + 1.0 / f64::from(self.coordinate_height)
        {
            return Err("reference screenshot aspect ratio is inconsistent".into());
        }
        Ok(())
    }
}
impl ZoomParams {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.sources.is_empty() || self.sources.len() > 4 {
            return Err("zoom requires 1..4 sources".into());
        }
        let count: usize = self.sources.iter().map(|s| s.regions.len()).sum();
        if count == 0 || count > MAX_REGIONS {
            return Err("zoom requires 1..16 regions in total".into());
        }
        let mut labels = std::collections::BTreeSet::new();
        for source in &self.sources {
            if source.regions.is_empty() {
                return Err("each source requires regions".into());
            }
            if source.image.as_ref().is_some_and(|i| i.handle.len() > 128) {
                return Err("invalid image handle length".into());
            }
            if source.image.is_some()
                && (source.target.is_some()
                    || source.reference.is_some()
                    || source.raise_window.is_some())
            {
                return Err(
                    "prior-image sources cannot specify target, reference, or raise_window".into(),
                );
            }
            if source.target.as_ref().is_some_and(|t| !t.has_target()) {
                return Err("empty window target; omit target for desktop".into());
            }
            if let Some(reference) = &source.reference {
                reference.validate()?;
            }
            for region in &source.regions {
                if region.label.trim().is_empty()
                    || region.label.len() > 128
                    || !labels.insert(&region.label)
                {
                    return Err("region labels must be unique and contain 1..128 bytes".into());
                }
                if !(1..=8).contains(&region.factor.unwrap_or(2)) {
                    return Err("factor must be 1..8".into());
                }
                if region.rect.is_some() == region.element_index.is_some() {
                    return Err("choose exactly one of rect or element_index".into());
                }
                if region.rect.is_some() && source.image.is_none() && source.reference.is_none() {
                    return Err("fresh rectangles require reference screenshot geometry".into());
                }
            }
        }
        Ok(())
    }
}
pub(crate) fn source_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("encoded source exceeds 16 MiB".into());
    }
    let (width, height, bpp) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 26 {
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
            if bytes[24] == 16 { 8u64 } else { 4u64 },
        )
    } else {
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let (width, height) = reader
            .into_dimensions()
            .map_err(|e| format!("source header failed: {e}"))?;
        (width, height, 4)
    };
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS
        || u64::from(width) * u64::from(height) * bpp > 256 * 1024 * 1024
    {
        return Err("source header exceeds dimension/pixel budget before decode allocation".into());
    }
    Ok((width, height))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    source_dimensions(bytes)?;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(MAX_SOURCE_PIXELS * 4);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| format!("source decode failed: {e}"))
}

pub(crate) fn map_rect(rect: &Rect, from: (u32, u32), to: (u32, u32)) -> Result<Rect, String> {
    rect.validate(from.0, from.1)?;
    let x = u64::from(rect.x) * u64::from(to.0) / u64::from(from.0);
    let y = u64::from(rect.y) * u64::from(to.1) / u64::from(from.1);
    let right = (u64::from(rect.x + rect.width) * u64::from(to.0)).div_ceil(u64::from(from.0));
    let bottom = (u64::from(rect.y + rect.height) * u64::from(to.1)).div_ceil(u64::from(from.1));
    let mapped = Rect {
        x: x as u32,
        y: y as u32,
        width: (right - x) as u32,
        height: (bottom - y) as u32,
    };
    mapped.validate(to.0, to.1)?;
    Ok(mapped)
}

pub(crate) fn selection(pixels: &Pixels, region: &Region) -> Result<(Rect, u32, u32), String> {
    let dimensions = pixels.image.dimensions();
    let rect = match (&region.rect, region.element_index) {
        (Some(rect), None) => map_rect(
            rect,
            (pixels.geometry.width, pixels.geometry.height),
            dimensions,
        )?,
        (None, Some(index)) => {
            let rect =
                pixels.association.elements.get(&index).ok_or(
                    "element bounds unavailable or unverified for this image/window scope",
                )?;
            map_rect(
                rect,
                (
                    pixels.geometry.coordinate_width,
                    pixels.geometry.coordinate_height,
                ),
                dimensions,
            )?
        }
        _ => return Err("choose exactly one selection".into()),
    };
    let factor = region.factor.unwrap_or(2);
    let width = rect
        .width
        .checked_mul(factor)
        .ok_or("output width overflow")?;
    let height = rect
        .height
        .checked_mul(factor)
        .ok_or("output height overflow")?;
    if width > 4096 || height > 4096 || u64::from(width) * u64::from(height) > MAX_OUTPUT_PIXELS {
        return Err("enlarged output exceeds 4096 dimensions or 16 Mi pixels".into());
    }
    Ok((rect, width, height))
}

#[cfg(test)]
pub(crate) fn crop(pixels: &Pixels, region: &Region) -> Result<(Vec<u8>, Value), String> {
    crop_bounded(
        pixels,
        region,
        MAX_IMAGE_BYTES,
        &crate::zoom_processing::Control::new(std::time::Duration::from_secs(45)),
    )
}

pub(crate) fn crop_bounded(
    pixels: &Pixels,
    region: &Region,
    cap: usize,
    control: &crate::zoom_processing::Control,
) -> Result<(Vec<u8>, Value), String> {
    control.check()?;
    let dimensions = pixels.image.dimensions();
    let (rect, width, height) = selection(pixels, region)?;
    let factor = region.factor.unwrap_or(2);
    let bytes = crate::zoom_processing::enlarge_png(&pixels.image, &rect, factor, cap, control)?;
    let association = compose_association(pixels, &rect, (width, height), factor)?;
    let metadata = json!({"label":region.label,"crop_rect":rect,"source_dimensions":{"width":dimensions.0,"height":dimensions.1},
        "reference":pixels.geometry,"coordinate_width":width,"coordinate_height":height,"zoom_association":association,"selection":{"rect":region.rect,"element_index":region.element_index},"window_scope":pixels.association.window.as_ref().map(|window| json!({"window_id":window.window_id,"backend":window.backend})),"factor":factor,"width":width,"height":height,"format":"png","bit_depth":pixels.image.color().bits_per_pixel()/u16::from(pixels.image.color().channel_count()),"filter":"nearest",
        "retained_source":pixels.retained,"detail_note":if pixels.retained {"Retained encoded pixels only; enlargement cannot recover missing details."} else {"Original captured pixels cropped before preview downscaling."},
        "transform":{"output_to_source_scale":1.0/f64::from(factor),"output_to_source_offset":[rect.x,rect.y],
            "source_to_coordinate_scale":[f64::from(pixels.geometry.coordinate_width)/f64::from(dimensions.0),f64::from(pixels.geometry.coordinate_height)/f64::from(dimensions.1)],
            "coordinate_to_capture_offset":pixels.association.origin,"capture_dimensions":pixels.association.full_dimensions,"output_to_capture_scale":association.capture_scale,"output_to_capture_offset":association.capture_offset},"bytes":bytes.len()});
    Ok((bytes, metadata))
}

fn compose_association(
    pixels: &Pixels,
    rect: &Rect,
    output: (u32, u32),
    factor: u32,
) -> Result<Association, String> {
    let sx = f64::from(pixels.geometry.coordinate_width) / f64::from(pixels.image.width());
    let sy = f64::from(pixels.geometry.coordinate_height) / f64::from(pixels.image.height());
    let old = &pixels.association;
    let offset = old
        .capture_offset
        .unwrap_or([f64::from(old.origin.0), f64::from(old.origin.1)]);
    let mut association = Association {
        window: old.window.clone(),
        origin: old.origin,
        full_dimensions: old.full_dimensions,
        frame_object_ref: old.frame_object_ref.clone(),
        capture_scale: [
            old.capture_scale[0] * sx / f64::from(factor),
            old.capture_scale[1] * sy / f64::from(factor),
        ],
        capture_offset: Some([
            offset[0] + f64::from(rect.x) * sx * old.capture_scale[0],
            offset[1] + f64::from(rect.y) * sy * old.capture_scale[1],
        ]),
        ..Default::default()
    };
    for (&index, element) in &old.elements {
        let mapped = map_rect(
            element,
            (
                pixels.geometry.coordinate_width,
                pixels.geometry.coordinate_height,
            ),
            pixels.image.dimensions(),
        )?;
        if mapped.x < rect.x
            || mapped.y < rect.y
            || mapped.x + mapped.width > rect.x + rect.width
            || mapped.y + mapped.height > rect.y + rect.height
        {
            continue;
        }
        let rebased = Rect {
            x: (mapped.x - rect.x) * factor,
            y: (mapped.y - rect.y) * factor,
            width: mapped.width * factor,
            height: mapped.height * factor,
        };
        rebased.validate(output.0, output.1)?;
        association.elements.insert(index, rebased);
        if let Some(identity) = old.identities.get(&index) {
            association.identities.insert(index, identity.clone());
        }
    }
    Ok(association)
}

pub(crate) struct Output {
    pub regions: Vec<Value>,
    pub images: Vec<Content>,
    pub(crate) bytes: usize,
    pub(crate) work_pixels: u64,
    byte_exhausted: bool,
    pub failed: bool,
}

impl Output {
    pub(crate) fn new() -> Self {
        Self {
            regions: vec![],
            images: vec![],
            bytes: 0,
            work_pixels: 0,
            byte_exhausted: false,
            failed: false,
        }
    }
    pub(crate) fn error(&mut self, source: usize, label: &str, error: String) {
        self.failed = true;
        self.regions
            .push(json!({"source_index":source,"label":label,"ok":false,"error":error}));
    }
    #[cfg(test)]
    pub(crate) fn add(&mut self, source: usize, pixels: &Pixels, region: &Region) {
        let control = crate::zoom_processing::Control::new(std::time::Duration::from_secs(45));
        let result = self
            .reserve_work(pixels, region)
            .and_then(|()| crop_bounded(pixels, region, MAX_IMAGE_BYTES - self.bytes, &control));
        self.add_processed(source, &region.label, result);
    }
    pub(crate) fn reserve_work(&mut self, pixels: &Pixels, region: &Region) -> Result<(), String> {
        if self.byte_exhausted || self.bytes >= MAX_IMAGE_BYTES {
            return Err("zoom total image byte budget exhausted (4 MiB)".into());
        }
        let (_, width, height) = selection(pixels, region)?;
        let work = u64::from(width) * u64::from(height);
        if self.work_pixels + work > 64 * 1024 * 1024 {
            return Err("zoom cumulative output work exceeds 64 Mi pixels".into());
        }
        self.work_pixels += work;
        Ok(())
    }
    pub(crate) fn add_processed(
        &mut self,
        source: usize,
        label: &str,
        result: Result<(Vec<u8>, Value), String>,
    ) {
        match result {
            Ok((bytes, mut metadata)) => {
                self.bytes += bytes.len();
                metadata["source_index"] = json!(source);
                metadata["ok"] = json!(true);
                metadata["image"] = json!({"content_index":self.images.len()+1});
                self.regions.push(metadata);
                self.images
                    .push(Content::image(STANDARD.encode(bytes), "image/png"));
            }
            Err(error) => {
                if error.contains("PNG byte budget exhausted") {
                    self.byte_exhausted = true;
                }
                self.error(source, label, error);
            }
        }
    }
    pub(crate) fn finish(self) -> CallToolResult {
        let metadata = json!({"ok":!self.failed,"regions":self.regions});
        let mut content = vec![Content::text(metadata.to_string())];
        content.extend(self.images);
        let mut result = if self.failed {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        result.structured_content = Some(metadata);
        result
    }
}

#[cfg(test)]
#[path = "zoom-pixel-tests.rs"]
pub(crate) mod tests;
