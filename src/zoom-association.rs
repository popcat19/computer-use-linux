// Purpose: Bind accessibility bounds to one verified screenshot window and pixel coordinate space.

use crate::{
    atspi_tree::AccessibilityNode,
    windowing::WindowInfo,
    zoom::{Association, ElementIdentity, Rect},
};
use std::collections::BTreeMap;

pub(crate) fn associate(
    nodes: &[AccessibilityNode],
    window: &WindowInfo,
    logical: (i32, i32, u32, u32),
    physical: (i32, i32, u32, u32),
    crop: (i32, i32, u32, u32),
    dimensions: (u32, u32),
) -> Association {
    let mut association = Association {
        window: Some(window.clone()),
        origin: (crop.0, crop.1),
        full_dimensions: dimensions,
        elements: BTreeMap::new(),
        ..Default::default()
    };
    // A single top-level frame must establish both scope and units. Containment alone cannot distinguish logical from physical coordinates.
    let roots: Vec<_> = nodes
        .iter()
        .filter(|node| {
            matches!(node.role.as_str(), "frame" | "dialog" | "window")
                && node.bounds.as_ref().is_some_and(|bounds| {
                    let rect = (
                        bounds.x,
                        bounds.y,
                        bounds.width as u32,
                        bounds.height as u32,
                    );
                    bounds.width > 0 && bounds.height > 0 && (rect == logical || rect == physical)
                })
        })
        .collect();
    if roots.len() != 1 {
        return association;
    }
    let root = roots[0];
    association.frame_object_ref = Some(root.object_ref.clone());
    let bounds = root.bounds.as_ref().unwrap();
    let input = (
        bounds.x,
        bounds.y,
        bounds.width as u32,
        bounds.height as u32,
    );
    let by_index: BTreeMap<_, _> = nodes.iter().map(|node| (node.index, node)).collect();
    for node in nodes {
        let mut current = Some(node.index);
        let mut belongs = false;
        for _ in 0..=nodes.len() {
            match current {
                Some(index) if index == root.index => {
                    belongs = true;
                    break;
                }
                Some(index) => current = by_index.get(&index).and_then(|node| node.parent_index),
                None => break,
            }
        }
        if !belongs {
            continue;
        }
        let Some(bounds) = &node.bounds else {
            continue;
        };
        if bounds.width <= 0 || bounds.height <= 0 {
            continue;
        }
        let x = i64::from(bounds.x) - i64::from(input.0);
        let y = i64::from(bounds.y) - i64::from(input.1);
        if x < 0
            || y < 0
            || x + i64::from(bounds.width) > i64::from(input.2)
            || y + i64::from(bounds.height) > i64::from(input.3)
        {
            continue;
        }
        let left = i64::from(physical.0) + x * i64::from(physical.2) / i64::from(input.2)
            - i64::from(crop.0);
        let top = i64::from(physical.1) + y * i64::from(physical.3) / i64::from(input.3)
            - i64::from(crop.1);
        let right = i64::from(physical.0)
            + ((x + i64::from(bounds.width)) * i64::from(physical.2) + i64::from(input.2) - 1)
                / i64::from(input.2)
            - i64::from(crop.0);
        let bottom = i64::from(physical.1)
            + ((y + i64::from(bounds.height)) * i64::from(physical.3) + i64::from(input.3) - 1)
                / i64::from(input.3)
            - i64::from(crop.1);
        if left < 0 || top < 0 || right > i64::from(crop.2) || bottom > i64::from(crop.3) {
            continue;
        }
        association.identities.insert(
            node.index,
            ElementIdentity {
                object_ref: node.object_ref.clone(),
                role: node.role.clone(),
                name: node.name.clone(),
            },
        );
        association.elements.insert(
            node.index,
            Rect {
                x: left as u32,
                y: top as u32,
                width: (right - left) as u32,
                height: (bottom - top) as u32,
            },
        );
    }
    association
}

pub(crate) fn refresh(
    old: &Association,
    nodes: &[AccessibilityNode],
    window: &WindowInfo,
    logical: (i32, i32, u32, u32),
    physical: (i32, i32, u32, u32),
    crop: (i32, i32, u32, u32),
    dimensions: (u32, u32),
) -> Association {
    let current = associate(nodes, window, logical, physical, crop, dimensions);
    let mut refreshed = Association {
        window: current.window.clone(),
        origin: current.origin,
        full_dimensions: current.full_dimensions,
        frame_object_ref: current.frame_object_ref.clone(),
        ..Default::default()
    };
    if old.frame_object_ref.is_none() || old.frame_object_ref != current.frame_object_ref {
        return refreshed;
    }
    for (&index, identity) in &old.identities {
        let matches: Vec<_> = current
            .identities
            .iter()
            .filter(|(_, candidate)| *candidate == identity)
            .collect();
        if matches.len() != 1 {
            continue;
        }
        let (&new_index, _) = matches[0];
        if let Some(bounds) = current.elements.get(&new_index) {
            refreshed.elements.insert(index, bounds.clone());
            refreshed.identities.insert(index, identity.clone());
        }
    }
    refreshed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atspi_tree::Bounds;
    fn node(
        index: u32,
        parent: Option<u32>,
        role: &str,
        rect: (i32, i32, i32, i32),
    ) -> AccessibilityNode {
        AccessibilityNode {
            index,
            parent_index: parent,
            depth: 0,
            object_ref: format!("fixture:{index}"),
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
        serde_json::from_value(serde_json::json!({"window_id":1,"title":"fixture","app_id":"fixture","wm_class":null,"pid":123,"bounds":{"x":-50,"y":20,"width":100,"height":80},"workspace":null,"focused":true,"hidden":false,"client_type":"wayland","backend":"fixture"})).unwrap()
    }
    #[test]
    fn logical_origin_scale_and_scope_are_verified_by_unique_frame() {
        let nodes = vec![
            node(0, None, "frame", (-50, 20, 100, 80)),
            node(1, Some(0), "button", (-40, 30, 10, 10)),
            node(2, None, "button", (-40, 30, 10, 10)),
        ];
        let association = associate(
            &nodes,
            &window(),
            (-50, 20, 100, 80),
            (100, 40, 200, 160),
            (100, 40, 200, 160),
            (400, 300),
        );
        assert_eq!(
            association.elements[&1],
            Rect {
                x: 20,
                y: 20,
                width: 20,
                height: 20
            }
        );
        assert!(!association.elements.contains_key(&2));
        assert_eq!(association.origin, (100, 40));
    }
    #[test]
    fn physical_units_and_clipped_origin_do_not_guess() {
        let nodes = vec![
            node(0, None, "frame", (-20, 40, 200, 160)),
            node(1, Some(0), "button", (20, 60, 20, 20)),
            node(2, Some(0), "button", (-20, 60, 10, 10)),
        ];
        let association = associate(
            &nodes,
            &window(),
            (-50, 20, 100, 80),
            (-20, 40, 200, 160),
            (0, 40, 180, 160),
            (400, 300),
        );
        assert_eq!(
            association.elements[&1],
            Rect {
                x: 20,
                y: 20,
                width: 20,
                height: 20
            }
        );
        assert!(!association.elements.contains_key(&2));
    }
    #[test]
    fn ambiguous_or_missing_calibration_rejects_all_elements() {
        let logical = (-50, 20, 100, 80);
        let physical = (100, 40, 200, 160);
        for nodes in [
            vec![node(1, None, "button", (-40, 30, 10, 10))],
            vec![
                node(0, None, "frame", logical_as_i32(logical)),
                node(1, None, "frame", logical_as_i32(logical)),
            ],
        ] {
            assert!(
                associate(&nodes, &window(), logical, physical, physical, (400, 300))
                    .elements
                    .is_empty()
            );
        }
    }
    fn logical_as_i32(rect: (i32, i32, u32, u32)) -> (i32, i32, i32, i32) {
        (rect.0, rect.1, rect.2 as i32, rect.3 as i32)
    }
}
