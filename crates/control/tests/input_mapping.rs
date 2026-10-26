use serde_json::{json, Value};
use std::time::Instant;
use vm_control::input::{plan, source_pixel_arguments, CoordinateError, Frames, FRAME_TTL};

fn frame(width: u64, height: u64) -> Value {
    json!({"frame_id":"00000000-0000-0000-0000-000000000000",
        "width":width,"height":height,"source_generation":4})
}

fn click(screenshot: &Value, x: u64, y: u64) -> Vec<Vec<Value>> {
    let now = Instant::now();
    let mut frames = Frames::default();
    frames.record(screenshot, now);
    let mut args = frame(
        screenshot["width"].as_u64().unwrap(),
        screenshot["height"].as_u64().unwrap(),
    );
    args["x"] = json!(x);
    args["y"] = json!(y);
    let mut current = screenshot.clone();
    current["capture_connected"] = json!(true);
    current["frame_available"] = json!(true);
    frames.check(&args, &current, now).unwrap();
    plan("input_click", &args).unwrap()
}

fn assert_click(steps: Vec<Vec<Value>>, x: u64, y: u64) {
    assert_eq!(
        steps,
        vec![
            vec![
                json!({"type":"abs","data":{"axis":"x","value":x}}),
                json!({"type":"abs","data":{"axis":"y","value":y}}),
            ],
            vec![json!({"type":"btn","data":{"button":"left","down":true}})],
            vec![json!({"type":"btn","data":{"button":"left","down":false}})],
        ]
    );
}

fn retina_frame() -> Value {
    let mut screenshot = frame(2560, 1440);
    screenshot["point_pixel_scale"] = json!(2);
    screenshot["coordinate_space"] = json!("guest-display-physical-pixels");
    screenshot
}

#[test]
fn input_mapping_1x_uses_source_pixel_centres() {
    assert_click(click(&frame(1280, 720), 200, 150), 5132, 6849);
}

#[test]
fn input_mapping_2x_requires_caller_to_double_points() {
    let screenshot = retina_frame();
    assert_click(click(&screenshot, 400, 300), 5126, 6838);
    // Screenshot scale metadata does not convert point-valued input coordinates.
    assert_click(click(&screenshot, 200, 150), 2566, 3424);
}

#[test]
fn input_mapping_2x_point_binding_should_convert_to_source_pixels() {
    let mut args = frame(2560, 1440);
    args["x"] = json!(200);
    args["y"] = json!(150);
    args["coordinate_space"] = json!("points");
    args["point_pixel_scale"] = json!(2.0);
    let pixels = source_pixel_arguments("input_click", &args).unwrap();
    assert_eq!(
        (pixels["x"].clone(), pixels["y"].clone()),
        (json!(400), json!(300))
    );
    assert_click(plan("input_click", &args).unwrap(), 5126, 6838);
}

#[test]
fn input_mapping_letterboxed_destination_requires_preconverted_source_pixels() {
    let screenshot = frame(2560, 1440);
    // DisplayedFrameTests.m: 1920x1200 backing, viewport (0,60,1920,1080),
    // unflipped view. Native mapping gives these source pixels; Rust consumes
    // them without a viewport transform. This does not execute the native helper.
    for (x, y, absolute_x, absolute_y) in [
        (1280, 720, 16390, 16395),  // backing (960,600), centre
        (0, 0, 6, 11),              // backing (0,1140), top left
        (2559, 1439, 32761, 32756), // backing (1919.5,60.5), bottom right
    ] {
        assert_click(click(&screenshot, x, y), absolute_x, absolute_y);
    }
    // Passing backing coordinates directly neither removes bars nor flips y.
    assert_click(click(&screenshot, 960, 600), 12294, 13664);
    assert_click(click(&screenshot, 0, 1140), 6, 25952);
}

#[test]
fn input_mapping_destination_bars_and_edges_are_not_source_bounds() {
    let screenshot = frame(2560, 1440);
    // Native helper rejects these viewport/bar points (DisplayedFrameTests.m).
    // Rust has only source bounds, so each is accepted as a source pixel.
    for (x, y, absolute_x, absolute_y) in [
        (1920, 600, 24582, 13664),
        (960, 60, 12294, 1376),
        (960, 1170, 12294, 26635),
    ] {
        assert_click(click(&screenshot, x, y), absolute_x, absolute_y);
    }
}

#[test]
fn input_mapping_source_bounds_reject_without_clamping() {
    for method in ["input_click", "input_drag", "input_scroll"] {
        for (field, value, message) in [
            ("x", json!(2560), "coordinate outside frame"),
            ("y", json!(1440), "coordinate outside frame"),
            ("x", json!(u64::MAX), "coordinate outside frame"),
            ("y", json!(u64::MAX), "coordinate outside frame"),
            ("x", json!(-1), "x must be nonnegative integer"),
            ("y", json!(-1), "y must be nonnegative integer"),
            ("x", json!(0.5), "x must be nonnegative integer"),
            ("y", json!(0.5), "y must be nonnegative integer"),
        ] {
            let mut args = frame(2560, 1440);
            args["x"] = json!(0);
            args["y"] = json!(0);
            args["end_x"] = json!(2559);
            args["end_y"] = json!(1439);
            args["direction"] = json!("down");
            args["ticks"] = json!(1);
            args[field] = value;
            assert_eq!(
                plan(method, &args).unwrap_err().to_string(),
                message,
                "{method}: {field}"
            );
        }
    }
    for (field, value) in [("end_x", 2560), ("end_y", 1440)] {
        let mut args = frame(2560, 1440);
        args["x"] = json!(0);
        args["y"] = json!(0);
        args["end_x"] = json!(2559);
        args["end_y"] = json!(1439);
        args[field] = json!(value);
        assert_eq!(
            plan("input_drag", &args).unwrap_err().to_string(),
            "coordinate outside frame"
        );
    }
}

#[test]
fn input_mapping_binding_rejects_point_dimensions_for_a_pixel_screenshot() {
    let now = Instant::now();
    let screenshot = retina_frame();
    let mut frames = Frames::default();
    frames.record(&screenshot, now);
    let mut current = screenshot.clone();
    current["capture_connected"] = json!(true);
    current["frame_available"] = json!(true);
    for (field, points) in [("width", 1280), ("height", 720)] {
        let mut args = screenshot.clone();
        args[field] = json!(points);
        assert_eq!(
            frames.check(&args, &current, now).unwrap_err().to_string(),
            format!("frame geometry or generation changed: {field}")
        );
    }
}

fn pointer_args() -> Value {
    let mut args = frame(2560, 1440);
    args["x"] = json!(200);
    args["y"] = json!(150);
    args["end_x"] = json!(1000);
    args["end_y"] = json!(700);
    args["direction"] = json!("down");
    args["ticks"] = json!(3);
    args
}

#[test]
fn input_mapping_default_and_explicit_pixels_preserve_wire_bytes() {
    for method in ["input_click", "input_drag", "input_scroll"] {
        let args = pointer_args();
        let bytes = serde_json::to_vec(&args).unwrap();
        assert_eq!(
            serde_json::to_vec(source_pixel_arguments(method, &args).unwrap().as_ref()).unwrap(),
            bytes
        );
        let legacy = plan(method, &args).unwrap();
        let mut explicit = args.clone();
        explicit["coordinate_space"] = json!("source_pixels");
        assert_eq!(
            serde_json::to_vec(&plan(method, &explicit).unwrap()).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
        let mut points = args.clone();
        points["coordinate_space"] = json!("points");
        points["point_pixel_scale"] = json!(1.0);
        assert_eq!(
            serde_json::to_vec(source_pixel_arguments(method, &points).unwrap().as_ref()).unwrap(),
            bytes
        );
        assert_eq!(
            serde_json::to_vec(&plan(method, &points).unwrap()).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
    }
    // Frozen legacy event bytes: the new options do not change an omitted-field request.
    assert_eq!(
        serde_json::to_string(&plan("input_click", &pointer_args()).unwrap()).unwrap(),
        r#"[[{"data":{"axis":"x","value":2566},"type":"abs"},{"data":{"axis":"y","value":3424},"type":"abs"}],[{"data":{"button":"left","down":true},"type":"btn"}],[{"data":{"button":"left","down":false},"type":"btn"}]]"#
    );
}

#[test]
fn input_mapping_coordinate_options_have_typed_rejections() {
    for method in ["input_click", "input_drag", "input_scroll"] {
        for space in [
            json!("unknown"),
            json!("guest-display-physical-pixels"),
            Value::Null,
            json!(7),
            json!(false),
        ] {
            let mut args = pointer_args();
            args["coordinate_space"] = space;
            let error = plan(method, &args).unwrap_err();
            assert_eq!(
                error.downcast_ref::<CoordinateError>(),
                Some(&CoordinateError::InvalidSpace)
            );
            assert!(error.to_string().starts_with("invalid_coordinate_space:"));
        }
        for space in [None, Some(json!("source_pixels"))] {
            for scale in [json!(2), Value::Null, json!(0)] {
                let mut args = pointer_args();
                if let Some(space) = &space {
                    args["coordinate_space"] = space.clone();
                }
                args["point_pixel_scale"] = scale;
                assert_eq!(
                    plan(method, &args)
                        .unwrap_err()
                        .downcast_ref::<CoordinateError>(),
                    Some(&CoordinateError::ScaleNotAllowed)
                );
            }
        }
        for scale in [
            None,
            Some(Value::Null),
            Some(json!(0)),
            Some(json!(-0.0)),
            Some(json!(-1)),
            Some(json!(8.000001)),
            Some(json!("2.0")),
            Some(json!(false)),
            Some(json!([])),
            Some(json!(f64::NAN)),
            Some(json!(f64::INFINITY)),
            Some(json!(f64::NEG_INFINITY)),
        ] {
            let mut args = pointer_args();
            args["coordinate_space"] = json!("points");
            if let Some(scale) = scale {
                args["point_pixel_scale"] = scale;
            }
            let error = plan(method, &args).unwrap_err();
            assert_eq!(
                error.downcast_ref::<CoordinateError>(),
                Some(&CoordinateError::InvalidScale)
            );
            assert!(error.to_string().starts_with("invalid_point_pixel_scale:"));
        }
    }
    // JSON has no non-finite numbers: serde_json maps constructed NaN/infinities to null,
    // and refuses their raw spellings (including a numeric overflow) at the wire boundary.
    for raw in ["NaN", "Infinity", "-Infinity", "1e400"] {
        assert!(
            serde_json::from_str::<Value>(&format!(r#"{{"point_pixel_scale":{raw}}}"#)).is_err()
        );
    }
}

#[test]
fn input_mapping_points_round_ties_away_from_zero_without_clamping() {
    for (x, scale, pixel) in [
        (0.25, 2.0, 1),
        (1.25, 2.0, 3),
        (1.24, 2.0, 2),
        (1.26, 2.0, 3),
        (0.0625, 8.0, 1),
    ] {
        let mut args = pointer_args();
        args["x"] = json!(x);
        args["y"] = json!(x);
        args["coordinate_space"] = json!("points");
        args["point_pixel_scale"] = json!(scale);
        let pixels = source_pixel_arguments("input_click", &args).unwrap();
        assert_eq!(pixels["x"], pixel);
        assert_eq!(pixels["y"], pixel);
        assert_eq!(
            plan("input_click", &args).unwrap(),
            click(&frame(2560, 1440), pixel, pixel)
        );
    }
    for field in ["x", "y", "end_x", "end_y"] {
        for bad in [
            json!(-0.01),
            json!(-0.25),
            Value::Null,
            json!("1"),
            json!(false),
            json!(f64::MAX),
        ] {
            let mut args = pointer_args();
            args["coordinate_space"] = json!("points");
            args["point_pixel_scale"] = json!(2.0);
            args[field] = bad;
            assert_eq!(
                plan("input_drag", &args)
                    .unwrap_err()
                    .downcast_ref::<CoordinateError>(),
                Some(&CoordinateError::InvalidPoint(field))
            );
        }
        let mut args = pointer_args();
        args["coordinate_space"] = json!("points");
        args["point_pixel_scale"] = json!(2.0);
        args.as_object_mut().unwrap().remove(field);
        assert_eq!(
            plan("input_drag", &args)
                .unwrap_err()
                .downcast_ref::<CoordinateError>(),
            Some(&CoordinateError::InvalidPoint(field))
        );
        args[field] = if field.ends_with('x') {
            json!(1279.75)
        } else {
            json!(719.75)
        };
        assert_eq!(
            plan("input_drag", &args).unwrap_err().to_string(),
            "coordinate outside frame"
        );
    }
}

#[test]
fn input_mapping_drag_and_scroll_convert_positions_only() {
    for method in ["input_drag", "input_scroll"] {
        for direction in ["up", "down", "left", "right"] {
            for ticks in [1, 3, 20] {
                let mut args = pointer_args();
                args["direction"] = json!(direction);
                args["ticks"] = json!(ticks);
                let mut pixels = args.clone();
                for field in ["x", "y", "end_x", "end_y"] {
                    pixels[field] = json!(args[field].as_u64().unwrap() * 2);
                }
                args["coordinate_space"] = json!("points");
                args["point_pixel_scale"] = json!(2.0);
                let steps = plan(method, &args).unwrap();
                assert_eq!(steps, plan(method, &pixels).unwrap());
                if method == "input_scroll" {
                    let legacy = {
                        let mut a = args.clone();
                        a.as_object_mut().unwrap().remove("coordinate_space");
                        a.as_object_mut().unwrap().remove("point_pixel_scale");
                        plan(method, &a).unwrap()
                    };
                    assert_eq!(&steps[1..], &legacy[1..]);
                    assert_eq!(steps.len(), 1 + 2 * ticks as usize);
                }
            }
        }
    }
}

#[test]
fn input_mapping_points_keep_pixel_frame_fences_and_ignore_screenshot_scale() {
    let now = Instant::now();
    let mut screenshot = retina_frame();
    screenshot["point_pixel_scale"] = json!(7); // Deliberately unlike the caller's scale.
    let mut frames = Frames::default();
    frames.record(&screenshot, now);
    let mut current = screenshot.clone();
    current["capture_connected"] = json!(true);
    current["frame_available"] = json!(true);
    let mut args = pointer_args();
    args["coordinate_space"] = json!("points");
    args["point_pixel_scale"] = json!(2.0);
    let pixels = source_pixel_arguments("input_click", &args).unwrap();
    assert_eq!(pixels["x"], 400);
    frames.check(&pixels, &current, now).unwrap();
    assert!(frames
        .check(
            &pixels,
            &current,
            now + FRAME_TTL + std::time::Duration::from_millis(1)
        )
        .is_err());
    assert!(Frames::default().check(&pixels, &current, now).is_err());
    for field in ["width", "height", "source_generation"] {
        let mut changed = current.clone();
        changed[field] = json!(999);
        assert_eq!(
            frames
                .check(&pixels, &changed, now)
                .unwrap_err()
                .to_string(),
            format!("frame geometry or generation changed: {field}")
        );
    }
    for field in ["capture_connected", "frame_available"] {
        let mut unavailable = current.clone();
        unavailable[field] = json!(false);
        assert!(frames.check(&pixels, &unavailable, now).is_err());
    }
}
