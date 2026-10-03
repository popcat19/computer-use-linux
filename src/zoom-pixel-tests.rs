// Purpose: Prove screenshot pixel mappings, enlargement fidelity and bounded region outputs with synthetic PNGs.

use super::*;
use image::ImageFormat;

pub(crate) fn patterned(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([x as u8, y as u8, ((x + y) % 2 * 255) as u8, 255])
    }))
}
fn region(label: &str, rect: Rect) -> Region {
    Region {
        label: label.into(),
        rect: Some(rect),
        element_index: None,
        factor: None,
    }
}
pub(crate) fn png(image: &DynamicImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}
#[test]
fn original_crop_maps_preview_and_nearest_preserves_checkerboard() {
    let pixels = Pixels {
        image: patterned(16, 12),
        geometry: Geometry {
            width: 8,
            height: 6,
            coordinate_width: 16,
            coordinate_height: 12,
        },
        association: Association::default(),
        retained: false,
    };
    let (bytes, meta) = crop(
        &pixels,
        &region(
            "defect",
            Rect {
                x: 2,
                y: 1,
                width: 3,
                height: 2,
            },
        ),
    )
    .unwrap();
    assert_eq!(meta["crop_rect"], json!({"x":4,"y":2,"width":6,"height":4}));
    let output = decode(&bytes).unwrap();
    assert_eq!(output.dimensions(), (12, 8));
    for y in 0..8 {
        for x in 0..12 {
            assert_eq!(
                output.get_pixel(x, y),
                pixels.image.get_pixel(4 + x / 2, 2 + y / 2)
            );
        }
    }
}
#[test]
fn rounded_preview_transform_encloses_exact_edges() {
    assert_eq!(
        map_rect(
            &Rect {
                x: 1,
                y: 1,
                width: 1,
                height: 1
            },
            (3, 3),
            (10, 10)
        )
        .unwrap(),
        Rect {
            x: 3,
            y: 3,
            width: 4,
            height: 4
        }
    );
}
#[test]
fn retained_crop_uses_only_existing_pixels() {
    let retained = Retained {
        encoded: None,
        bytes: png(&patterned(8, 6)),
        geometry: Geometry {
            width: 8,
            height: 6,
            coordinate_width: 16,
            coordinate_height: 12,
        },
        association: Association::default(),
    }
    .decode()
    .unwrap();
    let (_, meta) = crop(
        &retained,
        &region(
            "old",
            Rect {
                x: 2,
                y: 1,
                width: 3,
                height: 2,
            },
        ),
    )
    .unwrap();
    assert_eq!(meta["crop_rect"], json!({"x":2,"y":1,"width":3,"height":2}));
    assert_eq!(meta["retained_source"], true);
    assert!(meta["detail_note"]
        .as_str()
        .unwrap()
        .contains("cannot recover"));
}
#[test]
fn output_keeps_multiple_sources_labels_and_partial_native_images() {
    let pixels = Pixels {
        image: patterned(10, 10),
        geometry: Geometry {
            width: 10,
            height: 10,
            coordinate_width: 10,
            coordinate_height: 10,
        },
        association: Association::default(),
        retained: false,
    };
    let mut output = Output::new();
    output.add(
        0,
        &pixels,
        &region(
            "first",
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        ),
    );
    output.add(
        0,
        &pixels,
        &region(
            "second",
            Rect {
                x: 2,
                y: 2,
                width: 2,
                height: 2,
            },
        ),
    );
    output.add(
        1,
        &pixels,
        &region(
            "third",
            Rect {
                x: 4,
                y: 4,
                width: 2,
                height: 2,
            },
        ),
    );
    output.add(
        2,
        &pixels,
        &region(
            "bad",
            Rect {
                x: 10,
                y: 0,
                width: 1,
                height: 1,
            },
        ),
    );
    let result = output.finish();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 4);
    let metadata = result.structured_content.unwrap();
    assert_eq!(metadata["regions"][2]["source_index"], 1);
    assert_eq!(metadata["regions"][3]["ok"], false);
    let text = &result.content[0].as_text().unwrap().text;
    assert!(!text.contains("base64"));
    for (index, block) in result.content.iter().skip(1).enumerate() {
        let image = decode(&STANDARD.decode(&block.as_image().unwrap().data).unwrap()).unwrap();
        assert_eq!(
            image.get_pixel(0, 0),
            pixels.image.get_pixel(index as u32 * 2, index as u32 * 2)
        );
    }
}
#[test]
fn bounds_overflow_dimensions_and_decode_limits_refuse() {
    for rect in [
        Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        },
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        Rect {
            x: 9,
            y: 0,
            width: 2,
            height: 1,
        },
    ] {
        assert!(rect.validate(10, 10).is_err());
    }
    assert!(decode(&vec![0; MAX_SOURCE_BYTES + 1]).is_err());
    let pixels = Pixels {
        image: patterned(520, 1),
        geometry: Geometry {
            width: 520,
            height: 1,
            coordinate_width: 520,
            coordinate_height: 1,
        },
        association: Association::default(),
        retained: false,
    };
    let mut region = region(
        "large",
        Rect {
            x: 0,
            y: 0,
            width: 520,
            height: 1,
        },
    );
    region.factor = Some(8);
    assert!(crop(&pixels, &region).is_err());
    assert!(Geometry {
        width: 100,
        height: 2,
        coordinate_width: 100,
        coordinate_height: 100
    }
    .validate()
    .is_err());
}
#[test]
fn request_validation_is_complete_before_capture() {
    let valid = json!({"sources":[{"reference":{"width":10,"height":10,"coordinate_width":20,"coordinate_height":20},"regions":[{"label":"a","rect":{"x":1,"y":1,"width":2,"height":2}}]}]});
    assert!(serde_json::from_value::<ZoomParams>(valid.clone())
        .unwrap()
        .validate()
        .is_ok());
    for changed in [
        json!({"sources":[]}),
        json!({"sources":[{"regions":[{"label":"a","rect":{"x":1,"y":1,"width":2,"height":2}}]}]}),
        json!({"sources":[{"image":{"$image":"x"},"target":{"window_id":1},"regions":[{"label":"a","element_index":1}]}]}),
    ] {
        assert!(serde_json::from_value::<ZoomParams>(changed)
            .unwrap()
            .validate()
            .is_err());
    }
    for factor in [0, 9, u32::MAX] {
        let mut request = valid.clone();
        request["sources"][0]["regions"][0]["factor"] = json!(factor);
        assert!(serde_json::from_value::<ZoomParams>(request)
            .unwrap()
            .validate()
            .is_err());
    }
    let mut request = valid;
    request["sources"][0]["regions"] = Value::Array(
        (0..17)
            .map(|n| json!({"label":n.to_string(),"element_index":n}))
            .collect(),
    );
    assert!(serde_json::from_value::<ZoomParams>(request)
        .unwrap()
        .validate()
        .is_err());
}
#[test]
fn total_png_budget_keeps_prior_regions_without_text_fallback() {
    let mut state = 0x12345678u32;
    let image = image::RgbaImage::from_fn(1024, 400, |_, _| {
        let mut channels = [0; 4];
        for channel in &mut channels {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *channel = state as u8;
        }
        image::Rgba(channels)
    });
    let pixels = Pixels {
        image: DynamicImage::ImageRgba8(image),
        geometry: Geometry {
            width: 1024,
            height: 400,
            coordinate_width: 1024,
            coordinate_height: 400,
        },
        association: Association::default(),
        retained: false,
    };
    let mut output = Output::new();
    for label in ["one", "two", "three"] {
        let mut region = region(
            label,
            Rect {
                x: 0,
                y: 0,
                width: 1024,
                height: 400,
            },
        );
        region.factor = Some(1);
        output.add(0, &pixels, &region);
    }
    let work = output.work_pixels;
    let mut skipped = region(
        "skipped",
        Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: 400,
        },
    );
    skipped.factor = Some(1);
    output.add(0, &pixels, &skipped);
    assert_eq!(output.work_pixels, work);
    let result = output.finish();
    assert_eq!(result.content.len(), 3);
    assert_eq!(result.is_error, Some(true));
    assert!(result.structured_content.unwrap()["regions"][2]["error"]
        .as_str()
        .unwrap()
        .contains("4 MiB"));
    assert!(!result.content[0].as_text().unwrap().text.contains("base64"));
}
