// Purpose: Keep screenshot payloads out of JSON and retain native script images.

use rmcp::model::{CallToolResult, Content};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

const MAX_STORED_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_STORED_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_EMITTED_IMAGE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn state_result(mut state: Value) -> Result<CallToolResult, String> {
    let mut images = Vec::new();
    if let Some(screenshot) = state.get_mut("screenshot").and_then(Value::as_object_mut) {
        if let Some(data_url) = screenshot.remove("data_url") {
            let url = data_url.as_str().ok_or("invalid screenshot data URL")?;
            let (header, data) = url.split_once(",").ok_or("invalid screenshot data URL")?;
            let mime = header
                .strip_prefix("data:")
                .and_then(|header| header.strip_suffix(";base64"))
                .filter(|mime| matches!(*mime, "image/png" | "image/jpeg"))
                .ok_or("invalid screenshot MIME type")?;
            images.push(Content::image(data, mime));
            screenshot.insert("image".into(), json!({"content_index": 1}));
        }
    }
    let mut result = CallToolResult::success(vec![Content::text(state.to_string())]);
    result.content.extend(images);
    result.structured_content = Some(state);
    Ok(result)
}

pub(crate) struct ScriptMedia {
    scope: String,
    images: Vec<Content>,
    image_metadata: Vec<Value>,
    stored_bytes: usize,
    metadata_bytes: usize,
    emitted_bytes: usize,
    emitted: BTreeSet<usize>,
    omitted: usize,
}

impl Default for ScriptMedia {
    fn default() -> Self {
        Self {
            scope: {
                let mut nonce = [0u8; 16];
                let nonce = if getrandom::fill(&mut nonce).is_ok() {
                    u128::from_ne_bytes(nonce)
                } else {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                        ^ (u128::from(std::process::id()) << 64)
                };
                format!(
                    "cul-script-{nonce:032x}-{}:",
                    NEXT_SCOPE.fetch_add(1, Ordering::Relaxed)
                )
            },
            images: Vec::new(),
            image_metadata: Vec::new(),
            stored_bytes: 0,
            metadata_bytes: 0,
            emitted_bytes: 0,
            emitted: BTreeSet::new(),
            omitted: 0,
        }
    }
}

impl ScriptMedia {
    pub(crate) fn capture(&mut self, value: Value) -> Result<Value, String> {
        let result: CallToolResult =
            serde_json::from_value(value).map_err(|_| "invalid image-bearing tool result")?;
        let mut metadata = result
            .structured_content
            .or_else(|| {
                result
                    .content
                    .iter()
                    .filter_map(|block| block.as_text())
                    .find_map(|block| serde_json::from_str(&block.text).ok())
            })
            .ok_or("image-bearing tool did not return JSON metadata")?;
        if !metadata.is_object() {
            return Err("image metadata must be an object".into());
        }
        if result.is_error == Some(true) {
            metadata["ok"] = Value::Bool(false);
        }
        let mut handles = BTreeMap::new();
        let mut pending = Vec::new();
        let mut image_bytes = self.stored_bytes;
        let mut metadata_bytes = self.metadata_bytes;
        for (index, block) in result.content.into_iter().enumerate() {
            let Some(image) = block.as_image() else {
                continue;
            };
            let saved = find_image_metadata(&metadata, index).unwrap_or_else(|| metadata.clone());
            let saved_bytes = serde_json::to_vec(&saved)
                .map_err(|_| "invalid image metadata")?
                .len();
            if image_bytes + image.data.len() > MAX_STORED_IMAGE_BYTES
                || metadata_bytes + saved_bytes > MAX_STORED_METADATA_BYTES
            {
                if metadata.get("regions").is_some() && omit_zoom_image(&mut metadata, index) {
                    metadata["ok"] = Value::Bool(false);
                    continue;
                }
                return Err("script retained images exceed 16 MiB or image metadata exceeds 4 MiB; disable intermediate screenshots".into());
            }
            image_bytes += image.data.len();
            metadata_bytes += saved_bytes;
            let handle = self.images.len() + pending.len();
            pending.push((block, saved));
            handles.insert(index, json!({"$image":format!("{}{handle}",self.scope)}));
        }
        let replaced = replace_references(&mut metadata, &handles);
        if !replaced && !handles.is_empty() {
            metadata
                .as_object_mut()
                .unwrap()
                .insert("image".into(), handles.values().next().unwrap().clone());
        }
        for (block, saved) in pending {
            self.images.push(block);
            self.image_metadata.push(saved);
        }
        self.stored_bytes = image_bytes;
        self.metadata_bytes = metadata_bytes;
        Ok(metadata)
    }

    pub(crate) fn resolve(
        &self,
        reference: &crate::zoom::ImageRef,
    ) -> Result<crate::zoom::Retained, String> {
        let index = reference
            .handle
            .strip_prefix(&self.scope)
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or("invalid, stale, or foreign script image handle")?;
        let image = self
            .images
            .get(index)
            .and_then(|block| block.as_image())
            .ok_or("unknown script image handle")?;
        let metadata = self
            .image_metadata
            .get(index)
            .ok_or("missing image geometry")?;
        let width = metadata
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("missing image width")?;
        let height = metadata
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("missing image height")?;
        let geometry = crate::zoom::Geometry {
            width,
            height,
            coordinate_width: metadata
                .get("coordinate_width")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(width),
            coordinate_height: metadata
                .get("coordinate_height")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(height),
        };
        geometry.validate()?;
        let association = metadata
            .get("zoom_association")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| "invalid retained image association")?
            .unwrap_or_default();
        Ok(crate::zoom::Retained {
            encoded: Some(image.data.clone()),
            bytes: Vec::new(),
            geometry,
            association,
        })
    }

    pub(crate) fn emit(&mut self, value: Value) -> Vec<Content> {
        let mut content = vec![Content::text(value.to_string())];
        let mut handles = BTreeSet::new();
        collect_handles(&value, &mut handles, &self.scope);
        for handle in handles {
            if self.emitted.contains(&handle) {
                continue;
            }
            let Some(image) = self.images.get(handle) else {
                self.omitted += 1;
                continue;
            };
            let bytes = image.as_image().unwrap().data.len();
            if self.emitted_bytes + bytes > MAX_EMITTED_IMAGE_BYTES {
                self.omitted += 1;
                continue;
            }
            self.emitted_bytes += bytes;
            self.emitted.insert(handle);
            let metadata = &self.image_metadata[handle];
            content.push(Content::text(json!({"image_handle":format!("{}{handle}",self.scope),"label":metadata.get("label"),"source_index":metadata.get("source_index")}).to_string()));
            content.push(image.clone());
        }
        content
    }

    pub(crate) fn omitted(&self) -> usize {
        self.omitted
    }
}

fn omit_zoom_image(metadata: &mut Value, index: usize) -> bool {
    let Some(regions) = metadata.get_mut("regions").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(region) = regions.iter_mut().find(|region| {
        region
            .get("image")
            .and_then(|image| image.get("content_index"))
            .and_then(Value::as_u64)
            == Some(index as u64)
    }) else {
        return false;
    };
    if let Some(object) = region.as_object_mut() {
        object.remove("image");
        object.insert("ok".into(), Value::Bool(false));
        object.insert(
            "error".into(),
            json!("script retained image/metadata budget exceeds 16 MiB images or 4 MiB metadata; request fewer/smaller crops"),
        );
        return true;
    }
    false
}

fn find_image_metadata(value: &Value, index: usize) -> Option<Value> {
    if value
        .get("image")
        .and_then(|value| value.get("content_index"))
        .and_then(Value::as_u64)
        == Some(index as u64)
    {
        return Some(value.clone());
    }
    match value {
        Value::Object(object) => object
            .values()
            .find_map(|value| find_image_metadata(value, index)),
        Value::Array(array) => array
            .iter()
            .find_map(|value| find_image_metadata(value, index)),
        _ => None,
    }
}

fn replace_references(value: &mut Value, handles: &BTreeMap<usize, Value>) -> bool {
    if let Some(object) = value.as_object() {
        if object.len() == 1 {
            if let Some(handle) = object
                .get("content_index")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| handles.get(&index))
            {
                *value = handle.clone();
                return true;
            }
        }
    }
    let mut replaced = false;
    match value {
        Value::Object(object) => {
            for nested in object.values_mut() {
                replaced |= replace_references(nested, handles);
            }
        }
        Value::Array(array) => {
            for nested in array {
                replaced |= replace_references(nested, handles);
            }
        }
        _ => {}
    }
    replaced
}

fn collect_handles(value: &Value, handles: &mut BTreeSet<usize>, scope: &str) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                if object.len() == 1 {
                    if let Some(handle) = object
                        .get("$image")
                        .and_then(Value::as_str)
                        .and_then(|handle| handle.strip_prefix(scope))
                        .and_then(|handle| handle.parse::<usize>().ok())
                    {
                        handles.insert(handle);
                    }
                }
                pending.extend(object.values());
            }
            Value::Array(array) => pending.extend(array),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Value {
        json!({"screenshot": {"data_url": "data:image/png;base64,aGVsbG8=", "width": 10, "scale": 0.5}, "accessibility_tree": []})
    }

    #[test]
    fn state_contains_only_metadata_and_native_images() {
        let result = state_result(state()).unwrap();
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.content[1].as_image().unwrap().data, "aGVsbG8=");
        let text = &result.content[0].as_text().unwrap().text;
        assert!(!text.contains("base64"));
        assert!(!text.contains("aGVsbG8="));
        assert_eq!(
            result.structured_content.unwrap()["screenshot"]["scale"],
            0.5
        );
    }

    #[test]
    fn scripts_filter_json_and_emit_images_without_payload_strings() {
        let mut media = ScriptMedia::default();
        let state = media
            .capture(serde_json::to_value(state_result(state()).unwrap()).unwrap())
            .unwrap();
        assert!(state["screenshot"]["image"]["$image"]
            .as_str()
            .unwrap()
            .ends_with(":0"));
        assert!(!state.to_string().contains("aGVsbG8="));
        assert_eq!(media.emit(state["accessibility_tree"].clone()).len(), 1);
        let emitted = media.emit(state["screenshot"].clone());
        assert_eq!(emitted.len(), 3);
        assert!(emitted[2].as_image().is_some());
        assert_eq!(media.emit(state).len(), 1);
    }

    #[tokio::test]
    async fn rhai_filters_state_and_retains_its_selected_native_image() {
        let media = std::sync::Arc::new(std::sync::Mutex::new(ScriptMedia::default()));
        let capture = media.clone();
        let output = crate::run_script::execute_script(crate::run_script::ScriptParams {
            code: r#"let state = tools::invoke("get_app_state", #{}); emit(#{nodes: state.accessibility_tree.len(), image: state.screenshot.image});"#.into(),
            timeout_secs: None, max_calls: Some(1),
        }, move |_, _| {
            let capture = capture.clone();
            Box::pin(async move { capture.lock().unwrap().capture(serde_json::to_value(state_result(state()).unwrap()).unwrap()) })
        }).await;
        assert_eq!(output.error, None);
        assert_eq!(output.calls, 1);
        let content = media.lock().unwrap().emit(output.outputs[0].clone());
        assert!(content.iter().any(|block| block.as_image().is_some()));
        assert!(content
            .iter()
            .filter_map(|block| block.as_text())
            .all(|text| !text.text.contains("aGVsbG8=")));
        assert_eq!(output.outputs[0]["nodes"], 0);
    }

    #[test]
    fn handles_do_not_attach_images_from_another_script() {
        let mut first = ScriptMedia::default();
        let metadata = first
            .capture(serde_json::to_value(state_result(state()).unwrap()).unwrap())
            .unwrap();
        let mut second = ScriptMedia::default();
        second
            .capture(serde_json::to_value(state_result(state()).unwrap()).unwrap())
            .unwrap();
        assert_eq!(second.emit(metadata).len(), 1);
        assert_eq!(first.emit(json!({"$image":0})).len(), 1);
    }

    #[test]
    fn screenshot_caption_receives_a_handle() {
        let result = CallToolResult::success(vec![
            Content::image("aGVsbG8=", "image/png"),
            Content::text("{\"width\":10}"),
        ]);
        let mut media = ScriptMedia::default();
        let metadata = media
            .capture(serde_json::to_value(result).unwrap())
            .unwrap();
        assert_eq!(metadata["width"], 10);
        assert!(metadata["image"]["$image"].is_string());
        assert_eq!(media.emit(metadata).len(), 3);
    }

    #[test]
    fn image_budgets_are_explicit_without_text_fallback() {
        let mut media = ScriptMedia::default();
        let result = CallToolResult::success(vec![
            Content::image("A".repeat(MAX_EMITTED_IMAGE_BYTES + 1), "image/png"),
            Content::text("{}"),
        ]);
        let metadata = media
            .capture(serde_json::to_value(result).unwrap())
            .unwrap();
        let content = media.emit(metadata);
        assert_eq!(content.len(), 1);
        assert_eq!(media.omitted(), 1);
        assert!(content[0].as_text().unwrap().text.len() < 100);

        let result = CallToolResult::success(vec![
            Content::image("A".repeat(MAX_STORED_IMAGE_BYTES + 1), "image/png"),
            Content::text("{}"),
        ]);
        assert!(ScriptMedia::default()
            .capture(serde_json::to_value(result).unwrap())
            .is_err());
    }

    #[test]
    fn failed_workflow_feedback_keeps_image_handles() {
        let observed = state_result(state()).unwrap();
        let mut failed = CallToolResult::error(observed.content);
        failed.structured_content =
            Some(json!({"ok":false,"feedback":{},"state":observed.structured_content}));
        let mut media = ScriptMedia::default();
        let metadata = media
            .capture(serde_json::to_value(failed).unwrap())
            .unwrap();
        assert!(metadata["state"]["screenshot"]["image"]["$image"].is_string());
        assert!(media
            .emit(metadata)
            .iter()
            .any(|block| block.as_image().is_some()));
    }

    #[test]
    fn screenshot_free_state_and_ordinary_maps_are_preserved() {
        let state = json!({"screenshot":null,"screenshot_error":"failed"});
        let result = state_result(state.clone()).unwrap();
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.structured_content, Some(state));
        let value = json!({"structuredContent":{"answer":42}});
        assert_eq!(
            ScriptMedia::default().emit(value.clone())[0]
                .as_text()
                .unwrap()
                .text,
            value.to_string()
        );
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;
    use crate::zoom::{
        tests::{patterned, png},
        ImageRef, Rect,
    };
    use base64::{engine::general_purpose::STANDARD, Engine};

    fn captured_state(index: u32, x: u32) -> CallToolResult {
        state_result(json!({"screenshot":{"width":8,"height":6,"coordinate_width":16,"coordinate_height":12,
            "data_url":format!("data:image/png;base64,{}",STANDARD.encode(png(&patterned(8,6)))),
            "zoom_association":{"window":null,"origin":[100,40],"full_dimensions":[400,300],"elements":{index.to_string():{"x":x,"y":2,"width":4,"height":4}}}}})).unwrap()
    }
    #[test]
    fn image_association_survives_later_state_and_handles_refuse_foreign_stale() {
        let mut media = ScriptMedia::default();
        let old = media
            .capture(serde_json::to_value(captured_state(7, 4)).unwrap())
            .unwrap();
        media
            .capture(serde_json::to_value(captured_state(7, 10)).unwrap())
            .unwrap();
        let reference: ImageRef =
            serde_json::from_value(old["screenshot"]["image"].clone()).unwrap();
        let pixels = media.resolve(&reference).unwrap().decode().unwrap();
        assert_eq!(
            pixels.association.elements[&7],
            Rect {
                x: 4,
                y: 2,
                width: 4,
                height: 4
            }
        );
        let region = crate::zoom::Region {
            label: "old element".into(),
            rect: None,
            element_index: Some(7),
            factor: None,
        };
        let (bytes, meta) = crate::zoom::crop(&pixels, &region).unwrap();
        assert_eq!(meta["crop_rect"]["x"], 2);
        use image::GenericImageView;
        assert_eq!(
            crate::zoom::decode(&bytes).unwrap().get_pixel(0, 0),
            pixels.image.get_pixel(2, 1)
        );
        assert!(ScriptMedia::default().resolve(&reference).is_err());
        assert!(media
            .resolve(&ImageRef {
                handle: format!("{}9999", media.scope)
            })
            .is_err());
        assert!(media
            .resolve(&ImageRef {
                handle: "invalid".into()
            })
            .is_err());
    }
    #[tokio::test]
    async fn script_zoom_handles_native_images_and_stops_after_partial_error() {
        let media = std::sync::Arc::new(std::sync::Mutex::new(ScriptMedia::default()));
        let capture = media.clone();
        let output=crate::run_script::execute_script(crate::run_script::ScriptParams {
            code:r#"let state = tools::invoke("get_app_state", #{}); let z = tools::invoke("zoom", #{sources:[#{image:state.screenshot.image, regions:[#{label:"good",element_index:7},#{label:"bad",element_index:999}]}]}); tools::invoke("later", #{});"#.into(),timeout_secs:None,max_calls:Some(3)
        },move |name,args| {
            let capture=capture.clone();
            Box::pin(async move {
                let result=match name.as_str() {
                    "get_app_state"=> captured_state(7,4),
                    "zoom"=> {
                        let params:crate::zoom::ZoomParams=serde_json::from_value(args).unwrap(); params.validate()?;
                        let mut output=crate::zoom::Output::new();
                        for (index,source) in params.sources.iter().enumerate() {
                            let pixels=capture.lock().unwrap().resolve(source.image.as_ref().unwrap())?.decode()?;
                            for region in &source.regions {output.add(index,&pixels,region);}
                        }
                        output.finish()
                    }
                    _=> panic!("script must stop before later call"),
                };
                capture.lock().unwrap().capture(serde_json::to_value(result).unwrap())
            })
        }).await;
        assert_eq!(output.calls, 2);
        assert!(output.error.is_some());
        assert_eq!(output.outputs.len(), 1);
        let content = media.lock().unwrap().emit(output.outputs[0].clone());
        assert!(content.iter().any(|block| block.as_image().is_some()));
        assert!(content
            .iter()
            .filter_map(|block| block.as_text())
            .all(|text| !text.text.contains("base64")));
    }
    #[test]
    fn zoom_retention_budget_keeps_earlier_native_crops_and_labels_failure() {
        let mut media = ScriptMedia {
            stored_bytes: MAX_STORED_IMAGE_BYTES - 10,
            ..Default::default()
        };
        let metadata = json!({"ok":true,"regions":[{"label":"first","ok":true,"image":{"content_index":1}}, {"label":"second","ok":true,"image":{"content_index":2}}]});
        let mut result = CallToolResult::success(vec![
            Content::text(metadata.to_string()),
            Content::image("aGVsbG8=", "image/png"),
            Content::image("aGVsbG8=", "image/png"),
        ]);
        result.structured_content = Some(metadata);
        let metadata = media
            .capture(serde_json::to_value(result).unwrap())
            .unwrap();
        assert_eq!(metadata["ok"], false);
        assert!(metadata["regions"][0]["image"]["$image"].is_string());
        assert_eq!(metadata["regions"][1]["ok"], false);
        assert!(metadata["regions"][1].get("image").is_none());
        assert!(metadata["regions"][1]["error"]
            .as_str()
            .unwrap()
            .contains("16 MiB"));
        assert_eq!(
            media
                .emit(metadata)
                .iter()
                .filter(|block| block.as_image().is_some())
                .count(),
            1
        );
    }
    #[test]
    fn failed_non_zoom_capture_is_transactional_across_repeated_calls() {
        let mut media = ScriptMedia {
            stored_bytes: MAX_STORED_IMAGE_BYTES - 10,
            ..Default::default()
        };
        let result = CallToolResult::success(vec![
            Content::text("{}"),
            Content::image("aGVsbG8=", "image/png"),
            Content::image("aGVsbG8=", "image/png"),
        ]);
        for _ in 0..8 {
            assert!(media
                .capture(serde_json::to_value(&result).unwrap())
                .is_err());
        }
        assert!(media.images.is_empty());
        assert!(media.image_metadata.is_empty());
        assert_eq!(media.metadata_bytes, 0);
        assert_eq!(media.stored_bytes, MAX_STORED_IMAGE_BYTES - 10);
        let mut media = ScriptMedia {
            metadata_bytes: MAX_STORED_METADATA_BYTES,
            ..Default::default()
        };
        assert!(media
            .capture(serde_json::to_value(result).unwrap())
            .is_err());
        assert_eq!(media.stored_bytes, 0);
        assert!(media.images.is_empty());
    }
    #[test]
    fn nested_retained_preview_crops_compose_origin_scale_and_element_bounds() {
        let original = patterned(20, 12);
        let preview = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(4, 3, |x, y| {
            use image::GenericImageView;
            original.get_pixel(4 + x * 2, 2 + y * 2)
        }));
        let mut association = crate::zoom::Association {
            origin: (4, 2),
            full_dimensions: (20, 12),
            ..Default::default()
        };
        association.elements.insert(
            7,
            Rect {
                x: 2,
                y: 2,
                width: 2,
                height: 2,
            },
        );
        association.elements.insert(
            8,
            Rect {
                x: 5,
                y: 2,
                width: 2,
                height: 2,
            },
        );
        let saved=state_result(json!({"screenshot":{"width":4,"height":3,"coordinate_width":8,"coordinate_height":6,"zoom_association":association,"data_url":format!("data:image/png;base64,{}",STANDARD.encode(png(&preview)))}})).unwrap();
        let mut media = ScriptMedia::default();
        let state = media.capture(serde_json::to_value(saved).unwrap()).unwrap();
        let reference = serde_json::from_value(state["screenshot"]["image"].clone()).unwrap();
        let pixels = media.resolve(&reference).unwrap().decode().unwrap();
        let first = crate::zoom::Region {
            label: "first".into(),
            rect: Some(Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            }),
            element_index: None,
            factor: None,
        };
        let mut output = crate::zoom::Output::new();
        output.add(0, &pixels, &first);
        let first = media
            .capture(serde_json::to_value(output.finish()).unwrap())
            .unwrap();
        assert_eq!(
            first["regions"][0]["transform"]["output_to_capture_scale"],
            json!([1.0, 1.0])
        );
        assert_eq!(
            first["regions"][0]["transform"]["output_to_capture_offset"],
            json!([6.0, 4.0])
        );
        let reference = serde_json::from_value(first["regions"][0]["image"].clone()).unwrap();
        let pixels = media.resolve(&reference).unwrap().decode().unwrap();
        assert!(pixels.association.elements.contains_key(&7));
        assert!(!pixels.association.elements.contains_key(&8));
        let element = crate::zoom::Region {
            label: "element".into(),
            rect: None,
            element_index: Some(7),
            factor: None,
        };
        let (bytes, metadata) = crate::zoom::crop(&pixels, &element).unwrap();
        use image::GenericImageView;
        assert_eq!(
            crate::zoom::decode(&bytes).unwrap().get_pixel(0, 0),
            original.get_pixel(6, 4)
        );
        assert_eq!(
            metadata["transform"]["output_to_capture_scale"],
            json!([0.5, 0.5])
        );
        let second = crate::zoom::Region {
            label: "second".into(),
            rect: Some(Rect {
                x: 2,
                y: 0,
                width: 2,
                height: 2,
            }),
            element_index: None,
            factor: None,
        };
        let (bytes, metadata) = crate::zoom::crop(&pixels, &second).unwrap();
        assert_eq!(
            crate::zoom::decode(&bytes).unwrap().get_pixel(0, 0),
            original.get_pixel(8, 4)
        );
        assert_eq!(
            metadata["transform"]["output_to_capture_offset"],
            json!([8.0, 4.0])
        );
    }
    #[test]
    fn zoom_metadata_budget_omits_only_matching_label_and_nonobject_errors_do_not_mutate() {
        let metadata = json!({"ok":true,"regions":[{"label":"first","source_index":0,"image":{"content_index":1}},{"label":"second","source_index":1,"image":{"content_index":2},"zoom_association":{"large":"x".repeat(1000)}}]});
        let first_bytes = serde_json::to_vec(&metadata["regions"][0]).unwrap().len();
        let mut media = ScriptMedia {
            metadata_bytes: MAX_STORED_METADATA_BYTES - first_bytes - 1,
            ..Default::default()
        };
        let mut result = CallToolResult::success(vec![
            Content::text(metadata.to_string()),
            Content::image("aGVsbG8=", "image/png"),
            Content::image("aGVsbG8=", "image/png"),
        ]);
        result.structured_content = Some(metadata);
        let captured = media
            .capture(serde_json::to_value(result).unwrap())
            .unwrap();
        assert!(captured["regions"][0]["image"]["$image"].is_string());
        assert_eq!(captured["regions"][1]["ok"], false);
        assert!(captured["regions"][1].get("image").is_none());
        assert!(captured["regions"][1]["error"]
            .as_str()
            .unwrap()
            .contains("metadata"));
        assert_eq!(media.images.len(), 1);
        assert_eq!(media.metadata_bytes, MAX_STORED_METADATA_BYTES - 1);
        let mut media = ScriptMedia::default();
        let failed = CallToolResult::error(vec![
            Content::text("42"),
            Content::image("aGVsbG8=", "image/png"),
        ]);
        assert!(media
            .capture(serde_json::to_value(failed).unwrap())
            .is_err());
        assert!(media.images.is_empty());
        assert_eq!(media.stored_bytes, 0);
    }
}
