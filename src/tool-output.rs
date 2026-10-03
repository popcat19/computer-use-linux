// Purpose: Keep screenshot payloads out of JSON and retain native script images.

use rmcp::model::{CallToolResult, Content};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

const MAX_STORED_IMAGE_BYTES: usize = 16 * 1024 * 1024;
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
    stored_bytes: usize,
    emitted_bytes: usize,
    emitted: BTreeSet<usize>,
    omitted: usize,
}

impl Default for ScriptMedia {
    fn default() -> Self {
        Self {
            scope: format!("cul-script-{}:", NEXT_SCOPE.fetch_add(1, Ordering::Relaxed)),
            images: Vec::new(),
            stored_bytes: 0,
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
        if result.is_error == Some(true) {
            metadata["ok"] = Value::Bool(false);
        }
        let mut handles = BTreeMap::new();
        for (index, block) in result.content.into_iter().enumerate() {
            if let Some(image) = block.as_image() {
                self.stored_bytes += image.data.len();
                if self.stored_bytes > MAX_STORED_IMAGE_BYTES {
                    return Err("script retained images exceed 16 MiB; disable screenshots on intermediate observations".into());
                }
                let handle = self.images.len();
                self.images.push(block);
                handles.insert(index, json!({"$image": format!("{}{handle}", self.scope)}));
            }
        }
        let replaced = replace_references(&mut metadata, &handles);
        if !replaced && !handles.is_empty() {
            let object = metadata
                .as_object_mut()
                .ok_or("image metadata must be an object")?;
            object.insert("image".into(), handles.values().next().unwrap().clone());
        }
        Ok(metadata)
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
            content.push(Content::text(format!(
                "Image handle {}{handle}",
                self.scope
            )));
            content.push(image.clone());
        }
        content
    }

    pub(crate) fn omitted(&self) -> usize {
        self.omitted
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
