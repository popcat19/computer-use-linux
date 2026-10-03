// Purpose: Verify successful native and script MCP zoom routes with captured PNG fixtures and immutable object identities.

use super::*;
use crate::zoom::{
    tests::{patterned, png},
    Region,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::GenericImageView;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn node(
    index: u32,
    parent: Option<u32>,
    object: &str,
    role: &str,
    rect: (i32, i32, i32, i32),
) -> AccessibilityNode {
    AccessibilityNode {
        index,
        parent_index: parent,
        depth: 1,
        object_ref: object.into(),
        role: role.into(),
        name: None,
        description: None,
        child_count: 0,
        bounds: Some(Bounds {
            x: rect.0,
            y: rect.1,
            width: rect.2,
            height: rect.3,
        }),
        states: vec![],
        actions: vec![],
        value: None,
        text: None,
        supports_editable_text: false,
    }
}
fn window() -> WindowInfo {
    serde_json::from_value(json!({"window_id":123,"title":"fixture","app_id":"fixture","wm_class":null,"pid":123,"bounds":{"x":4,"y":2,"width":8,"height":6},"workspace":null,"focused":true,"hidden":false,"client_type":"wayland","backend":"fixture"})).unwrap()
}
fn nodes(x: i32, index: u32) -> Vec<AccessibilityNode> {
    vec![
        node(0, None, "frame:fixture", "frame", (4, 2, 8, 6)),
        node(index, Some(0), "button:fixture", "button", (x, 4, 2, 2)),
        node(2, Some(0), "other:fixture", "button", (9, 5, 2, 2)),
    ]
}
fn association() -> Association {
    crate::zoom_association::associate(
        &nodes(6, 1),
        &window(),
        (4, 2, 8, 6),
        (4, 2, 8, 6),
        (4, 2, 8, 6),
        (20, 12),
    )
}
fn fixture(x: i32, index: u32) -> Fixture {
    let bytes = png(&patterned(20, 12));
    Fixture {
        raw: RawScreenshotCapture {
            mime_type: "image/png".into(),
            bytes,
            source: "fixture".into(),
            width: 20,
            height: 12,
        },
        window: Some(window()),
        map: WindowCoordinateMap {
            capture_rect: (4, 2, 8, 6),
            full_capture_rect: (4, 2, 8, 6),
            portal_rect: Some((4, 2, 8, 6)),
        },
        nodes: nodes(x, index),
    }
}
fn server(fixtures: Vec<Fixture>) -> ComputerUseLinux {
    let server = ComputerUseLinux {
        zoom_fixtures: Some(Arc::new(Mutex::new(fixtures.into()))),
        ..Default::default()
    };
    *server.zoom_association.lock().unwrap() = association();
    server
}
fn image(result: &CallToolResult, index: usize) -> image::DynamicImage {
    let content = result
        .content
        .iter()
        .filter_map(|block| block.as_image())
        .nth(index)
        .unwrap();
    crate::zoom::decode(&STANDARD.decode(&content.data).unwrap()).unwrap()
}

#[tokio::test]
async fn moved_element_refreshes_same_object_not_new_index_and_retained_stays_immutable() {
    let server = server(vec![fixture(8, 9)]);
    let original = patterned(20, 12);
    let mut media = crate::tool_output::ScriptMedia::default();
    let saved=crate::tool_output::state_result(json!({"screenshot":{"width":8,"height":6,"coordinate_width":8,"coordinate_height":6,"zoom_association":association(),"data_url":format!("data:image/png;base64,{}",STANDARD.encode(png(&original.crop_imm(4,2,8,6))))}})).unwrap();
    let metadata = media.capture(serde_json::to_value(saved).unwrap()).unwrap();
    let fresh:ZoomParams=serde_json::from_value(json!({"sources":[{"target":{"window_id":123},"regions":[{"label":"fresh","element_index":1}]}]})).unwrap();
    let result = server.perform_zoom(fresh, None).await;
    assert_eq!(result.is_error, Some(false));
    assert_eq!(image(&result, 0).get_pixel(0, 0), original.get_pixel(8, 4));
    let reference = serde_json::from_value(metadata["screenshot"]["image"].clone()).unwrap();
    let pixels = media.resolve(&reference).unwrap().decode().unwrap();
    let (bytes, _) = crate::zoom::crop(
        &pixels,
        &Region {
            label: "retained".into(),
            element_index: Some(1),
            rect: None,
            factor: None,
        },
    )
    .unwrap();
    assert_eq!(
        crate::zoom::decode(&bytes).unwrap().get_pixel(0, 0),
        original.get_pixel(6, 4)
    );
    let mut replacement = nodes(8, 9);
    replacement[1].object_ref = "replaced:button".into();
    let refreshed = crate::zoom_association::refresh(
        &association(),
        &replacement,
        &window(),
        (4, 2, 8, 6),
        (4, 2, 8, 6),
        (4, 2, 8, 6),
        (20, 12),
    );
    assert!(!refreshed.elements.contains_key(&1));
}

#[tokio::test]
async fn mixed_regions_keep_valid_rectangle_and_error_only_missing_identity() {
    let server = server(vec![fixture(6, 1)]);
    let params:ZoomParams=serde_json::from_value(json!({"sources":[{"target":{"window_id":123},"raise_window":true,"reference":{"width":4,"height":3,"coordinate_width":8,"coordinate_height":6},"regions":[{"label":"good","rect":{"x":1,"y":1,"width":2,"height":1}},{"label":"missing","element_index":999},{"label":"oversized","rect":{"x":0,"y":0,"width":4000,"height":1}}]}]})).unwrap();
    let preflight = server.preflight_regions(&params.sources[0]);
    assert!(preflight[0].is_none());
    assert!(preflight[1].is_some());
    assert!(preflight[2].is_some());
    let result = server.perform_zoom(params, None).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .content
            .iter()
            .filter(|block| block.as_image().is_some())
            .count(),
        1
    );
    let metadata = result.structured_content.unwrap();
    assert_eq!(metadata["regions"][0]["ok"], true);
    assert_eq!(metadata["regions"][1]["ok"], false);
}

struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    id: u64,
}
impl Client {
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let request = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        self.writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == self.id {
                return value["result"].clone();
            }
        }
    }
}
#[tokio::test]
async fn actual_mcp_native_and_script_routes_preserve_nested_pixel_transforms() {
    let server = server(vec![
        fixture(6, 1),
        fixture(8, 9),
        fixture(6, 1),
        fixture(6, 1),
    ]);
    let (server_stream, client_stream) = tokio::io::duplex(1024 * 1024);
    let service = tokio::spawn(async move {
        server
            .serve(server_stream)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let (reader, writer) = tokio::io::split(client_stream);
    let mut client = Client {
        writer,
        reader: BufReader::new(reader),
        id: 0,
    };
    client.request("initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"zoom-fixture","version":"1"}})).await;
    client
        .writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let args = json!({"sources":[{"target":{"window_id":123},"reference":{"width":4,"height":3,"coordinate_width":8,"coordinate_height":6},"regions":[{"label":"first","rect":{"x":1,"y":1,"width":2,"height":1}}]}]});
    let native = client
        .request("tools/call", json!({"name":"zoom","arguments":args}))
        .await;
    let native_result: CallToolResult = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(native_result.is_error, Some(false));
    let metadata = native_result.structured_content.clone().unwrap();
    assert_eq!(
        metadata["regions"][0]["transform"]["output_to_capture_offset"],
        json!([6.0, 4.0])
    );
    assert_eq!(
        metadata["regions"][0]["transform"]["output_to_capture_scale"],
        json!([0.5, 0.5])
    );
    assert_eq!(
        image(&native_result, 0).get_pixel(0, 0),
        patterned(20, 12).get_pixel(6, 4)
    );
    assert!(!native_result.content[0]
        .as_text()
        .unwrap()
        .text
        .contains("base64"));
    if let Ok(path) = std::env::var("COMPUTER_USE_LINUX_ZOOM_RESULT_ARTIFACT") {
        std::fs::write(path, serde_json::to_vec_pretty(&native).unwrap()).unwrap();
    }
    let element=client.request("tools/call",json!({"name":"zoom","arguments":{"sources":[{"target":{"window_id":123},"regions":[{"label":"moved","element_index":1}]}]}})).await;
    let element: CallToolResult = serde_json::from_value(element).unwrap();
    assert_eq!(element.is_error, Some(false));
    assert_eq!(
        image(&element, 0).get_pixel(0, 0),
        patterned(20, 12).get_pixel(8, 4)
    );
    let script=client.request("tools/call",json!({"name":"run_script","arguments":{"code":r#"
        let first = tools::invoke("zoom", #{sources:[#{target:#{window_id:123},reference:#{width:4,height:3,coordinate_width:8,coordinate_height:6},regions:[#{label:"first",rect:#{x:1,y:1,width:2,height:1}}]}]});
        let second = tools::invoke("zoom", #{sources:[#{image:first.regions[0].image,regions:[#{label:"second",rect:#{x:2,y:2,width:2,height:2}},#{label:"element",element_index:1}]}]});
        emit(second);
    "#}})).await;
    let script: CallToolResult = serde_json::from_value(script).unwrap();
    assert_ne!(script.is_error, Some(true));
    assert_eq!(
        image(&script, 0).get_pixel(0, 0),
        patterned(20, 12).get_pixel(7, 5)
    );
    assert_eq!(
        image(&script, 1).get_pixel(0, 0),
        patterned(20, 12).get_pixel(6, 4)
    );
    let output: Value = script
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .find_map(|text| {
            serde_json::from_str::<Value>(&text.text)
                .ok()
                .filter(|value| value.get("regions").is_some())
        })
        .unwrap();
    assert_eq!(
        output["regions"][0]["transform"]["output_to_capture_offset"],
        json!([7.0, 5.0])
    );
    assert_eq!(
        output["regions"][0]["transform"]["output_to_capture_scale"],
        json!([0.25, 0.25])
    );
    assert_eq!(
        output["regions"][1]["zoom_association"]["elements"]["1"],
        json!({"x":0,"y":0,"width":8,"height":8})
    );
    assert!(output["regions"][0]["zoom_association"]["elements"]
        .get("2")
        .is_none());
    let failed=client.request("tools/call",json!({"name":"run_script","arguments":{"code":r#"tools::invoke("zoom", #{sources:[#{target:#{window_id:123},reference:#{width:4,height:3,coordinate_width:8,coordinate_height:6},regions:[#{label:"good",rect:#{x:1,y:1,width:2,height:1}},#{label:"missing",element_index:999}]}]}); tools::invoke("doctor", #{}); emit("must-not-run");"#}})).await;
    let failed: CallToolResult = serde_json::from_value(failed).unwrap();
    assert_eq!(failed.is_error, Some(true));
    let summary: Value = serde_json::from_str(&failed.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(summary["calls"], 1);
    assert!(failed
        .content
        .iter()
        .any(|block| block.as_image().is_some()));
    assert!(!serde_json::to_string(&failed)
        .unwrap()
        .contains("must-not-run"));
    let oversized = client
        .request(
            "tools/call",
            json!({"name":"run_script", "arguments":{"code":"emit(1);", "timeout_secs":u64::MAX}}),
        )
        .await;
    let oversized: CallToolResult = serde_json::from_value(oversized).unwrap();
    assert_eq!(oversized.is_error, Some(true));
    let summary: Value =
        serde_json::from_str(&oversized.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(summary["calls"], 0);
    assert!(summary["error"]
        .as_str()
        .unwrap()
        .contains("between 1 and 120"));
    let short=client.request("tools/call",json!({"name":"run_script","arguments":{"timeout_secs":1,"code":r#"tools::invoke("zoom", #{sources:[#{target:#{window_id:123},raise_window:true,reference:#{width:4,height:3,coordinate_width:8,coordinate_height:6},regions:[#{label:"deadline",rect:#{x:1,y:1,width:2,height:1}}]}]}); tools::invoke("doctor", #{});"#}})).await;
    let short: CallToolResult = serde_json::from_value(short).unwrap();
    assert_eq!(short.is_error, Some(true));
    assert!(!short.content.iter().any(|block| block.as_image().is_some()));
    assert!(short
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .any(|text| text.text.contains("deadline exceeded")));
    let summary: Value = serde_json::from_str(&short.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(summary["calls"], 1);
    drop(client);
    service.abort();
}
