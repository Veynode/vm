//! Read-only Guest window metadata. Neither a frame anchor nor input authority.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_BYTES: usize = 48 * 1024;
fn keys(v: &Value, fields: &[&str]) -> Result<()> {
    let object = v
        .as_object()
        .context("window observation object required")?;
    ensure!(
        object.len() == fields.len() && fields.iter().all(|k| object.contains_key(*k)),
        "invalid window observation fields"
    );
    Ok(())
}
fn uint(v: &Value, max: u64) -> Result<u64> {
    v.as_u64()
        .filter(|n| *n <= max)
        .context("invalid window observation integer")
}
fn positive(v: &Value, max: u64) -> Result<u64> {
    let n = uint(v, max)?;
    ensure!(n > 0, "window observation integer must be positive");
    Ok(n)
}
fn finite(v: &Value) -> Result<f64> {
    v.as_f64()
        .filter(|n| n.is_finite())
        .context("invalid window observation number")
}
fn bounds(v: &Value) -> Result<[f64; 4]> {
    keys(v, &["x", "y", "width", "height"])?;
    let b = [
        finite(&v["x"])?,
        finite(&v["y"])?,
        finite(&v["width"])?,
        finite(&v["height"])?,
    ];
    ensure!(
        b[2] > 0.0 && b[3] > 0.0 && (b[0] + b[2]).is_finite() && (b[1] + b[3]).is_finite(),
        "invalid window observation rectangle"
    );
    Ok(b)
}
fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}
fn boolean(v: &Value) -> Result<bool> {
    v.as_bool().context("invalid window observation boolean")
}

pub(crate) fn normalize(envelope: Value) -> Result<Value> {
    keys(&envelope, &["id", "result"])?;
    let value = &envelope["result"];
    ensure!(
        serde_json::to_vec(value)?.len() <= MAX_BYTES,
        "window observation exceeds 48 KiB"
    );
    keys(
        value,
        &[
            "version",
            "snapshot_id",
            "console_identity",
            "coordinate_space",
            "filter",
            "titles_included",
            "accessibility_tree",
            "input_authorization",
            "requested_at_us",
            "completed_at_us",
            "completed_monotonic_us",
            "observation_time_basis",
            "displays",
            "windows",
            "eligible_window_count",
            "omitted_invalid_count",
            "truncated",
        ],
    )?;
    ensure!(
        uint(&value["version"], 1)? == 1,
        "unsupported window observation version"
    );
    let id = value["snapshot_id"]
        .as_str()
        .context("window snapshot ID required")?;
    ensure!(
        id.len() == 36
            && id
                .bytes()
                .enumerate()
                .all(|(i, b)| if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }),
        "invalid window snapshot ID"
    );
    ensure!(
        value["coordinate_space"] == "guest-desktop-global-points"
            && value["filter"] == "on_screen_application_windows_layer_0"
            && value["observation_time_basis"]
                == "Guest enumeration completion; not an atomic desktop frame",
        "unsupported window observation semantics"
    );
    for field in [
        "titles_included",
        "accessibility_tree",
        "input_authorization",
    ] {
        ensure!(
            !boolean(&value[field])?,
            "unexpected window observation authority or private content"
        );
    }
    for field in [
        "requested_at_us",
        "completed_at_us",
        "completed_monotonic_us",
    ] {
        uint(&value[field], 9_007_199_254_740_991)?;
    }
    // Wall clock adjustment can move completed_at_us backwards. No freshness or
    // authority is inferred from either enumeration clock or this snapshot UUID.
    let identity = &value["console_identity"];
    keys(
        identity,
        &[
            "version",
            "security_session_id",
            "uid",
            "console_set",
            "display_id",
            "physical_width",
            "physical_height",
            "display_geometry_sha256",
        ],
    )?;
    ensure!(
        uint(&identity["version"], 1)? == 1,
        "console identity version"
    );
    positive(&identity["security_session_id"], u32::MAX as u64 - 2)?;
    positive(&identity["uid"], u32::MAX as u64 - 1)?;
    if !identity["console_set"].is_null() {
        uint(&identity["console_set"], u32::MAX as u64)?;
    }
    let primary = positive(&identity["display_id"], u32::MAX as u64)?;
    for name in ["physical_width", "physical_height"] {
        positive(&identity[name], 16_384)?;
    }
    ensure!(
        identity["display_geometry_sha256"]
            .as_str()
            .is_some_and(|s| crate::native_input::hex(s, 64)),
        "console geometry digest"
    );
    let displays = value["displays"]
        .as_array()
        .context("window displays required")?;
    ensure!(
        !displays.is_empty() && displays.len() <= 16,
        "window display count"
    );
    let mut display_ids = BTreeSet::new();
    let mut visible = std::collections::BTreeMap::new();
    let mut main_count = 0;
    for display in displays {
        keys(
            display,
            &[
                "display_id",
                "is_main",
                "active",
                "asleep",
                "bounds_points",
                "mode_pixel_width",
                "mode_pixel_height",
                "rotation_degrees",
                "point_pixel_scale",
            ],
        )?;
        let id = positive(&display["display_id"], u32::MAX as u64)?;
        ensure!(display_ids.insert(id), "duplicate window display ID");
        let main = boolean(&display["is_main"])?;
        let active = boolean(&display["active"])?;
        let asleep = boolean(&display["asleep"])?;
        if main {
            main_count += 1;
            ensure!(
                id == primary && active && !asleep,
                "console display is not eligible"
            );
        }
        let rect = bounds(&display["bounds_points"])?;
        for field in ["mode_pixel_width", "mode_pixel_height"] {
            positive(&display[field], u32::MAX as u64)?;
        }
        finite(&display["rotation_degrees"])?;
        if !display["point_pixel_scale"].is_null() {
            ensure!(
                finite(&display["point_pixel_scale"])? > 0.0,
                "invalid window point scale"
            );
        }
        if active && !asleep {
            visible.insert(id, rect);
        }
    }
    ensure!(
        main_count == 1,
        "missing or duplicate primary console display"
    );
    let windows = value["windows"]
        .as_array()
        .context("windows array required")?;
    let eligible = uint(&value["eligible_window_count"], 9_007_199_254_740_991)?;
    let omitted = uint(&value["omitted_invalid_count"], 9_007_199_254_740_991)?;
    let truncated = boolean(&value["truncated"])?;
    ensure!(
        windows.len() <= 128 && windows.len() as u64 <= eligible,
        "window count bound"
    );
    ensure!(
        truncated || (windows.len() as u64 == eligible && omitted == 0),
        "window omissions require explicit truncation"
    );
    let mut last = 0;
    for window in windows {
        keys(
            window,
            &["window_id", "owner", "bounds_points", "display_ids"],
        )?;
        let id = positive(&window["window_id"], u32::MAX as u64)?;
        ensure!(id > last, "window IDs must be unique and sorted");
        last = id;
        keys(
            &window["owner"],
            &["pid", "bundle_identifier", "application_name"],
        )?;
        positive(&window["owner"]["pid"], i32::MAX as u64)?;
        for field in ["bundle_identifier", "application_name"] {
            ensure!(
                window["owner"][field]
                    .as_str()
                    .is_some_and(|s| s.len() <= 256 && !s.contains('\0')),
                "window owner string bound"
            );
        }
        let rect = bounds(&window["bounds_points"])?;
        let referenced = window["display_ids"]
            .as_array()
            .context("window display references required")?;
        ensure!(
            !referenced.is_empty() && referenced.len() <= 16,
            "window display references bound"
        );
        let mut seen = BTreeSet::new();
        for display in referenced {
            let id = positive(display, u32::MAX as u64)?;
            ensure!(seen.insert(id), "duplicate window display reference");
            let bounds = visible
                .get(&id)
                .context("window references inactive or unknown display")?;
            ensure!(
                intersects(rect, *bounds),
                "window does not intersect referenced display"
            );
        }
        let expected: BTreeSet<_> = visible
            .iter()
            .filter_map(|(id, b)| intersects(rect, *b).then_some(*id))
            .collect();
        ensure!(seen == expected, "window display intersection mismatch");
    }
    if envelope["id"]
        .as_str()
        .is_none_or(|s| s.is_empty() || s.len() > 128)
    {
        bail!("invalid Guest window response ID");
    }
    Ok(value.clone())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    pub(crate) fn fixture() -> Value {
        json!({"id":"1","result":{"version":1,"snapshot_id":"11111111-1111-4111-8111-111111111111",
            "console_identity":{"version":1,"security_session_id":1,"uid":501,"console_set":null,"display_id":1,"physical_width":2560,"physical_height":1440,"display_geometry_sha256":"a".repeat(64)},
            "coordinate_space":"guest-desktop-global-points","filter":"on_screen_application_windows_layer_0",
            "titles_included":false,"accessibility_tree":false,"input_authorization":false,
            "requested_at_us":100,"completed_at_us":90,"completed_monotonic_us":200,
            "observation_time_basis":"Guest enumeration completion; not an atomic desktop frame",
            "displays":[{"display_id":1,"is_main":true,"active":true,"asleep":false,"bounds_points":{"x":-1280,"y":0,"width":1280,"height":720},"mode_pixel_width":2560,"mode_pixel_height":1440,"rotation_degrees":0,"point_pixel_scale":2}],
            "windows":[{"window_id":2,"owner":{"pid":123,"bundle_identifier":"app.test","application_name":"应用"},"bounds_points":{"x":-1200,"y":10,"width":800,"height":600},"display_ids":[1]}],
            "eligible_window_count":1,"omitted_invalid_count":0,"truncated":false}})
    }
    #[test]
    fn preserves_points_without_inventing_pixels_focus_or_authority() {
        let mut v = fixture();
        v["result"]["displays"][0]["point_pixel_scale"] = Value::Null;
        let actual = normalize(v).unwrap();
        assert_eq!(actual["windows"][0]["bounds_points"]["x"], -1200);
        assert!(actual["displays"][0]["point_pixel_scale"].is_null());
        assert_eq!(actual["input_authorization"], false);
    }
    #[test]
    fn rejects_private_unknown_fields_invalid_geometry_counts_and_types() {
        for (pointer, value) in [
            ("/result/windows/0/title", json!("private")),
            ("/result/windows/0/owner/pid", json!(true)),
            ("/result/displays/0/asleep", json!(true)),
            ("/result/displays/0/rotation_degrees", Value::Null),
            ("/result/windows/0/display_ids", json!([2])),
            ("/result/windows/0/bounds_points/x", json!(1)),
            ("/result/windows/0/bounds_points/width", json!(0)),
            (
                "/result/windows/0/owner/application_name",
                json!("界".repeat(86)),
            ),
            (
                "/result/completed_monotonic_us",
                json!(9_007_199_254_740_992u64),
            ),
            ("/result/titles_included", json!(true)),
            ("/result/eligible_window_count", json!(2)),
            ("/result/omitted_invalid_count", json!(1)),
        ] {
            let mut v = fixture();
            if pointer.ends_with("/title") {
                v["result"]["windows"][0]["title"] = value;
            } else {
                *v.pointer_mut(pointer).unwrap() = value;
            }
            assert!(normalize(v).is_err(), "{pointer}");
        }
        let mut v = fixture();
        v["result"]["windows"] = json!(vec![fixture()["result"]["windows"][0].clone(); 129]);
        assert!(normalize(v).is_err());
        let mut v = fixture();
        v["result"]["displays"] = json!(vec![fixture()["result"]["displays"][0].clone(); 17]);
        assert!(normalize(v).is_err());
        let mut v = fixture();
        v["result"]["extra"] = json!("x".repeat(MAX_BYTES));
        assert!(normalize(v).is_err());
    }
    #[test]
    fn spanning_window_requires_exact_awake_display_intersections() {
        let mut v = fixture();
        let mut second = v["result"]["displays"][0].clone();
        second["display_id"] = json!(2);
        second["is_main"] = json!(false);
        second["bounds_points"]["x"] = json!(0);
        second["point_pixel_scale"] = Value::Null;
        v["result"]["displays"].as_array_mut().unwrap().push(second);
        v["result"]["windows"][0]["bounds_points"]["width"] = json!(1600);
        v["result"]["windows"][0]["display_ids"] = json!([1, 2]);
        assert!(normalize(v.clone()).is_ok());
        v["result"]["windows"][0]["display_ids"] = json!([1]);
        assert!(normalize(v.clone()).is_err());
        v["result"]["displays"][1]["asleep"] = json!(true);
        assert!(normalize(v.clone()).is_ok());
        v["result"]["windows"][0]["display_ids"] = json!([1, 2]);
        assert!(normalize(v).is_err());
    }
    #[test]
    fn supports_explicit_truncation_but_not_successful_guest_errors() {
        let mut v = fixture();
        v["result"]["windows"] = json!([]);
        v["result"]["truncated"] = json!(true);
        v["result"]["omitted_invalid_count"] = json!(1);
        assert!(normalize(v).is_ok());
        assert!(normalize(json!({"id":"1","error":{"code":"forbidden"}})).is_err());
    }
}
