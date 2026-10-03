// Purpose: Verify fresh and retained source preflight, geometry consistency and labeled partial results without desktop effects.

use super::*;
use crate::zoom::{
    tests::{patterned, png},
    Retained,
};
use serde_json::json;
fn source(reference: serde_json::Value) -> Source {
    serde_json::from_value(json!({"reference":reference,"regions":[{"label":"fixture","rect":{"x":1,"y":1,"width":2,"height":2}}]})).unwrap()
}
#[test]
fn fresh_backend_fixture_rejects_changed_geometry_and_keeps_original_pixels() {
    let source = source(json!({"width":4,"height":3,"coordinate_width":8,"coordinate_height":6}));
    let pixels = fresh_pixels(&source, patterned(8, 6), Association::default()).unwrap();
    let (_, meta) = crate::zoom::crop(&pixels, &source.regions[0]).unwrap();
    assert_eq!(meta["crop_rect"], json!({"x":2,"y":2,"width":4,"height":4}));
    assert!(fresh_pixels(&source, patterned(9, 6), Association::default()).is_err());
}
#[tokio::test]
async fn retained_multiple_sources_partial_errors_and_no_desktop_calls() {
    let params:ZoomParams=serde_json::from_value(json!({"sources":[
            {"image":{"$image":"fixture:0"},"regions":[{"label":"good","rect":{"x":0,"y":0,"width":2,"height":2}}]},
            {"image":{"$image":"foreign"},"regions":[{"label":"bad","element_index":1}]}
        ]})).unwrap();
    let saved = Retained {
        encoded: None,
        bytes: png(&patterned(8, 6)),
        geometry: Geometry {
            width: 8,
            height: 6,
            coordinate_width: 8,
            coordinate_height: 6,
        },
        association: Association::default(),
    };
    let result = ComputerUseLinux::default()
        .perform_zoom(
            params,
            Some(vec![Ok(Some(saved)), Err("foreign handle".into())]),
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 2);
    assert!(result.content[1].as_image().is_some());
    assert_eq!(
        result.structured_content.unwrap()["regions"][1]["error"],
        "foreign handle"
    );
}
#[tokio::test]
async fn skipped_first_source_never_changes_later_retained_image_identity() {
    for invalid_handle in [false, true] {
        let params: ZoomParams = serde_json::from_value(json!({"sources":[
            {"image":{"$image":"first"},"regions":[{"label":"invalid","rect":{"x":99,"y":0,"width":1,"height":1}}]},
            {"image":{"$image":"second"},"regions":[{"label":"valid","rect":{"x":0,"y":0,"width":1,"height":1}}]}
        ]})).unwrap();
        let retained = |image| Retained {
            encoded: None,
            bytes: png(&image),
            geometry: Geometry {
                width: 8,
                height: 6,
                coordinate_width: 8,
                coordinate_height: 6,
            },
            association: Association::default(),
        };
        let first = if invalid_handle {
            Err("foreign handle".into())
        } else {
            Ok(Some(retained(patterned(8, 6))))
        };
        let second = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            6,
            image::Rgba([212, 33, 44, 255]),
        ));
        let result = ComputerUseLinux::default()
            .perform_zoom(params, Some(vec![first, Ok(Some(retained(second)))]))
            .await;
        assert_eq!(result.is_error, Some(true));
        let image = result
            .content
            .iter()
            .find_map(|block| block.as_image())
            .unwrap();
        use base64::Engine;
        use image::GenericImageView;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
        assert_eq!(
            crate::zoom::decode(&bytes).unwrap().get_pixel(0, 0),
            image::Rgba([212, 33, 44, 255])
        );
        let metadata = result.structured_content.unwrap();
        assert_eq!(metadata["regions"][1]["label"], "valid");
        assert_eq!(metadata["regions"][1]["source_index"], 1);
        assert_eq!(metadata["regions"][1]["ok"], true);
    }
}

#[tokio::test]
async fn standalone_prior_handle_and_invalid_request_refuse_before_capture() {
    let server = ComputerUseLinux::default();
    let value = json!({"sources":[{"image":{"$image":"foreign"},"regions":[{"label":"test","element_index":1}]}]});
    let result = server.dispatch_script_tool("zoom", value).await.unwrap();
    assert_eq!(result["isError"], true);
    assert!(result.to_string().contains("same run_script"));
    let result = server
        .perform_zoom(ZoomParams { sources: vec![] }, None)
        .await;
    assert_eq!(result.is_error, Some(true));
}
#[tokio::test]
async fn fresh_multi_window_fixture_captures_once_per_source_and_retains_failure_feedback() {
    let params:ZoomParams=serde_json::from_value(json!({"sources":[
            {"target":{"window_id":1},"reference":{"width":4,"height":3,"coordinate_width":8,"coordinate_height":6},"regions":[{"label":"one","rect":{"x":0,"y":0,"width":1,"height":1}},{"label":"two","rect":{"x":1,"y":1,"width":1,"height":1}}]},
            {"target":{"window_id":2},"reference":{"width":4,"height":3,"coordinate_width":8,"coordinate_height":6},"regions":[{"label":"three","rect":{"x":2,"y":1,"width":1,"height":1}}]},
            {"target":{"window_id":3},"regions":[{"label":"unresolved","element_index":1}]}
        ]})).unwrap();
    params.validate().unwrap();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = calls.clone();
    let result = collect_sources(&params.sources, move |_, source| {
        let id = source.target.as_ref().unwrap().window_id.unwrap();
        log.lock().unwrap().push(id);
        Box::pin(async move {
            if id == 3 {
                return Err("fixture target unresolved; no desktop fallback".into());
            }
            let decoded = crate::zoom::decode(&png(&patterned(8, 6))).unwrap();
            fresh_pixels(
                source,
                decoded,
                Association {
                    origin: ((id * 100) as i32, 40),
                    full_dimensions: (400, 300),
                    ..Default::default()
                },
            )
        })
    })
    .await;
    assert_eq!(*calls.lock().unwrap(), vec![1, 2, 3]);
    assert_eq!(
        result
            .content
            .iter()
            .filter(|block| block.as_image().is_some())
            .count(),
        3
    );
    let metadata = result.structured_content.unwrap();
    assert_eq!(
        metadata["regions"][2]["transform"]["coordinate_to_capture_offset"],
        json!([200, 40])
    );
    assert_eq!(metadata["regions"][3]["ok"], false);
}
#[test]
fn fresh_element_preflight_and_capture_association_reject_foreign_or_changed_scope() {
    let server = ComputerUseLinux::default();
    let window:WindowInfo=serde_json::from_value(json!({"window_id":1,"title":"fixture","app_id":"fixture","wm_class":null,"pid":123,"bounds":{"x":100,"y":40,"width":8,"height":6},"workspace":null,"focused":true,"hidden":false,"client_type":"wayland","backend":"fixture"})).unwrap();
    let mut cached = Association {
        window: Some(window.clone()),
        origin: (100, 40),
        full_dimensions: (400, 300),
        ..Default::default()
    };
    cached.identities.insert(
        7,
        crate::zoom::ElementIdentity {
            object_ref: "fixture:7".into(),
            role: "button".into(),
            name: None,
        },
    );
    cached.elements.insert(
        7,
        crate::zoom::Rect {
            x: 2,
            y: 2,
            width: 2,
            height: 2,
        },
    );
    *server.zoom_association.lock().unwrap() = cached.clone();
    for (id, index, valid) in [(1, 7, true), (2, 7, false), (1, 999, false)] {
        let source: Source = serde_json::from_value(
            json!({"target":{"window_id":id},"regions":[{"label":"test","element_index":index}]}),
        )
        .unwrap();
        assert_eq!(server.preflight_zoom_source(&source).is_ok(), valid);
    }
    assert!(
        association_for_capture(cached.clone(), &window, (100, 40), (400, 300))
            .elements
            .contains_key(&7)
    );
    assert!(
        association_for_capture(cached.clone(), &window, (101, 40), (400, 300))
            .elements
            .is_empty()
    );
    assert!(
        association_for_capture(cached.clone(), &window, (100, 40), (800, 600))
            .elements
            .is_empty()
    );
    let mut foreign = window.clone();
    foreign.window_id = 2;
    assert!(
        association_for_capture(cached.clone(), &foreign, (100, 40), (400, 300))
            .elements
            .is_empty()
    );
    let mut changed = window;
    changed.bounds.as_mut().unwrap().width = 9;
    assert!(
        association_for_capture(cached, &changed, (100, 40), (400, 300))
            .elements
            .is_empty()
    );
}
