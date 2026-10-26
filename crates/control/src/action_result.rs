//! The result of an input action, in cua-driver's `ActionResult` shape
//! (contract 0.8.0; docs/computer-use.md "Host plane"), so an agent reads
//! one model whether the Guest's driver or the host's virtual HID acted. The
//! host adds one route, `virtual_hid`, and one escalation reason,
//! `person_in_control` (the control lease).
//!
//! `check` holds the invariants every result must meet; host tools build
//! theirs with `host_input` from what the host saw after the input: a text
//! readback, or the screen change and where it was.
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub const EFFECTS: &[&str] = &[
    "confirmed",
    "partial",
    "unverifiable",
    "suspected_noop",
    "refused",
];
pub const ROUTES: &[&str] = &[
    "accessibility",
    "synthetic_events",
    "global_input",
    "system_api",
    "dom",
    "trusted_input",
    "virtual_hid",
];
const DELIVERY: &[&str] = &["background", "foreground", "not_applicable", "unknown"];
const EVIDENCE: &[&str] = &["value_readback", "window_change"];
const ESCALATION_REASONS: &[&str] = &[
    "route_unavailable",
    "delivery_failed",
    "effect_unconfirmed",
    "suspected_noop",
    "permission_required",
    "person_in_control",
];
const ESCALATION_TARGETS: &[&str] = &["pixel", "foreground", "page", "session", "lease"];

/// Checks an `ActionResult`: known values, and evidence that matches the
/// effect it claims.
pub fn check(result: &Value) -> Result<()> {
    let one_of = |key: &str, allowed: &[&str]| -> Result<Option<String>> {
        match result.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(Some(s.clone())),
            Some(other) => bail!("{key} {other} is not one of {}", allowed.join(", ")),
        }
    };
    let effect = one_of("effect", EFFECTS)?.ok_or_else(|| anyhow::anyhow!("effect is required"))?;
    one_of("route", ROUTES)?.ok_or_else(|| anyhow::anyhow!("route is required"))?;
    if let Some(delivery) = result.get("delivery").filter(|d| !d.is_null()) {
        match delivery["mode"].as_str() {
            Some(mode) if DELIVERY.contains(&mode) => {}
            _ => bail!("delivery.mode must be one of {}", DELIVERY.join(", ")),
        }
    }
    let evidence = match result.get("evidence") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => bail!("evidence must be an array"),
    };
    for item in &evidence {
        if !item["kind"].as_str().is_some_and(|k| EVIDENCE.contains(&k)) {
            bail!("evidence kind must be one of {}", EVIDENCE.join(", "))
        }
    }
    let escalation = result.get("escalation").filter(|e| !e.is_null());
    if let Some(escalation) = escalation {
        if !escalation["reason"]
            .as_str()
            .is_some_and(|r| ESCALATION_REASONS.contains(&r))
            || !escalation["target"]
                .as_str()
                .is_some_and(|t| ESCALATION_TARGETS.contains(&t))
        {
            bail!("escalation needs a known reason and target")
        }
    }
    if let Some(next) = result.get("next_call").filter(|n| !n.is_null()) {
        if !next["tool"].as_str().is_some_and(|t| !t.is_empty()) || !next["arguments"].is_object() {
            bail!("next_call needs a tool and an arguments object")
        }
    }
    let error = result.get("error").filter(|e| !e.is_null());
    if error.is_some_and(|e| !e["code"].as_str().is_some_and(|c| !c.is_empty())) {
        bail!("error needs a code")
    }
    match effect.as_str() {
        "confirmed" if evidence.is_empty() => bail!("a confirmed effect needs evidence"),
        "confirmed" if escalation.is_some() || error.is_some() => {
            bail!("a confirmed effect carries no escalation or error")
        }
        "refused" if error.is_none() && escalation.is_none() => {
            bail!("a refused action says why: error or escalation")
        }
        "unverifiable" | "suspected_noop" | "partial"
            if result.get("next_call").is_none_or(Value::is_null) && escalation.is_none() =>
        {
            bail!("an unconfirmed effect says what to do next: next_call or escalation")
        }
        _ => Ok(()),
    }
}

/// What the host saw after sending input.
pub enum Observed<'a> {
    /// The text the input should have produced was read back (OCR or a
    /// value read) and matched, or did not.
    Readback { matched: bool, detail: &'a str },
    /// The screen changed after the input; `at_target` when a changed region
    /// contains the point acted on.
    Changed { at_target: bool, regions: usize },
    /// The screen settled unchanged.
    Unchanged,
    /// Nothing was checked (no wait was asked for).
    Unchecked,
}

/// The `ActionResult` of input the host sent as virtual HID events.
/// `vm` names the VM for the suggested next call.
pub fn host_input(vm: &str, summary: &str, observed: Observed) -> Value {
    let observe = json!({"tool":"vm_screenshot","arguments":{"id":vm}});
    let mut result =
        json!({"route":"virtual_hid","delivery":{"mode":"foreground"},"summary":summary});
    match observed {
        Observed::Readback {
            matched: true,
            detail,
        } => {
            result["effect"] = json!("confirmed");
            result["evidence"] = json!([{"kind":"value_readback","detail":detail}]);
        }
        Observed::Readback {
            matched: false,
            detail,
        } => {
            result["effect"] = json!("partial");
            result["evidence"] = json!([{"kind":"value_readback","detail":detail}]);
            result["hint"] = json!(
                "The input was delivered but the text read back differs; look before repeating it."
            );
            result["next_call"] = observe;
        }
        Observed::Changed {
            at_target: true,
            regions,
        } => {
            result["effect"] = json!("confirmed");
            result["evidence"] = json!([{"kind":"window_change","detail":format!("{regions} changed region(s), one at the target")}]);
        }
        Observed::Changed {
            at_target: false,
            regions,
        } => {
            result["effect"] = json!("unverifiable");
            result["evidence"] = json!([{"kind":"window_change","detail":format!("{regions} changed region(s), none at the target")}]);
            result["hint"] = json!(
                "The screen changed, but not where the input went; look before acting again."
            );
            result["next_call"] = observe;
        }
        Observed::Unchanged => {
            result["effect"] = json!("suspected_noop");
            result["hint"] =
                json!("The screen settled unchanged; the input may have missed its target.");
            result["next_call"] = observe;
        }
        Observed::Unchecked => {
            result["effect"] = json!("unverifiable");
            result["hint"] =
                json!("Nothing checked the effect; read the screen before acting again.");
            result["next_call"] = observe;
        }
    }
    result
}

/// The point a host input tool acted on, in source pixels: the click or scroll
/// point, a drag's end. `None` for keys and text, and for points given in
/// another space, whose mapping to the screen this layer does not redo.
pub fn target(tool: &str, args: &Value) -> Option<(f64, f64)> {
    if args
        .get("coordinate_space")
        .is_some_and(|s| s != "source_pixels")
    {
        return None;
    }
    let (x, y) = match tool {
        "vm_input_drag" => ("end_x", "end_y"),
        "vm_input_click" | "vm_input_scroll" => ("x", "y"),
        _ => return None,
    };
    Some((args[x].as_f64()?, args[y].as_f64()?))
}

/// What a `screen_wait` after the input saw, for `host_input`. Without a
/// target point (keys, text) any change counts as at the target; with one,
/// only a changed region that contains it does.
pub fn observed_from_wait(
    wait: Option<&Value>,
    target: Option<(f64, f64)>,
    keyboard: bool,
) -> Observed<'static> {
    let Some(wait) = wait.filter(|w| w["changed"].is_boolean()) else {
        return Observed::Unchecked;
    };
    if wait["changed"] != true {
        return Observed::Unchanged;
    }
    let regions = wait["regions"].as_array();
    let count = regions.map_or(0, Vec::len);
    let at_target = match (target, regions) {
        (Some((x, y)), Some(regions)) => regions.iter().any(|r| {
            let (rx, ry) = (
                r["x"].as_f64().unwrap_or(f64::NAN),
                r["y"].as_f64().unwrap_or(f64::NAN),
            );
            let (w, h) = (
                r["width"].as_f64().unwrap_or(0.0),
                r["height"].as_f64().unwrap_or(0.0),
            );
            x >= rx && x < rx + w && y >= ry && y < ry + h
        }),
        (None, _) => keyboard,
        (Some(_), None) => false,
    };
    Observed::Changed {
        at_target,
        regions: count,
    }
}

/// A refusal because a person holds the control lease.
pub fn person_in_control(vm: &str, quiet_ms: u64) -> Value {
    json!({
        "effect":"refused","route":"virtual_hid","delivery":{"mode":"not_applicable"},
        "error":{"code":"person_in_control","hint":format!("A person is using this VM; agent input resumes {quiet_ms} ms after their last input.")},
        "escalation":{"target":"lease","reason":"person_in_control"},
        "next_call":{"tool":"vm_control_lease","arguments":{"id":vm,"action":"wait"}}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_host_result_meets_the_contract() {
        for observed in [
            Observed::Readback {
                matched: true,
                detail: "OCR reads 'hello'",
            },
            Observed::Readback {
                matched: false,
                detail: "OCR reads 'helo'",
            },
            Observed::Changed {
                at_target: true,
                regions: 2,
            },
            Observed::Changed {
                at_target: false,
                regions: 1,
            },
            Observed::Unchanged,
            Observed::Unchecked,
        ] {
            let result = host_input("vm1", "clicked at 10,20", observed);
            check(&result).unwrap_or_else(|e| panic!("{result}: {e}"));
            assert_eq!(result["route"], "virtual_hid");
        }
        check(&person_in_control("vm1", 5000)).unwrap();
    }

    #[test]
    fn a_wait_confirms_only_a_change_at_the_target() {
        let wait = json!({"changed":true,"regions":[{"x":0,"y":0,"width":100,"height":50}]});
        let click = target("vm_input_click", &json!({"x":10,"y":20})).unwrap();
        let miss = target("vm_input_click", &json!({"x":300,"y":20})).unwrap();
        assert!(matches!(
            observed_from_wait(Some(&wait), Some(click), false),
            Observed::Changed {
                at_target: true,
                regions: 1
            }
        ));
        assert!(matches!(
            observed_from_wait(Some(&wait), Some(miss), false),
            Observed::Changed {
                at_target: false,
                ..
            }
        ));
        // QEMU waits report no regions: a change cannot be placed.
        let qemu = json!({"changed":true,"regions":null});
        assert!(matches!(
            observed_from_wait(Some(&qemu), Some(click), false),
            Observed::Changed {
                at_target: false,
                ..
            }
        ));
        assert!(matches!(
            observed_from_wait(Some(&qemu), None, true),
            Observed::Changed {
                at_target: true,
                ..
            }
        ));
        assert!(matches!(
            observed_from_wait(Some(&json!({"changed":false})), None, true),
            Observed::Unchanged
        ));
        assert!(matches!(
            observed_from_wait(None, Some(click), false),
            Observed::Unchecked
        ));
        assert_eq!(
            target(
                "vm_input_click",
                &json!({"x":1,"y":2,"coordinate_space":"points"})
            ),
            None
        );
        assert_eq!(
            target("vm_input_drag", &json!({"x":1,"y":2,"end_x":5,"end_y":6})),
            Some((5.0, 6.0))
        );
    }

    #[test]
    fn only_evidence_confirms() {
        assert!(host_input("v", "s", Observed::Unchecked)["effect"] != "confirmed");
        assert!(host_input("v", "s", Observed::Unchanged)["effect"] != "confirmed");
        assert_eq!(
            host_input(
                "v",
                "s",
                Observed::Changed {
                    at_target: false,
                    regions: 3
                }
            )["effect"],
            "unverifiable"
        );
    }

    #[test]
    fn driver_results_from_phase_0_pass_and_broken_ones_fail() {
        // Replies cua-driver gave in phase 0 (vz-calls.jsonl).
        for good in [
            json!({"delivery":{"mode":"background"},"effect":"confirmed","evidence":[{"kind":"value_readback"}],"route":"accessibility","summary":"Set value"}),
            json!({"delivery":{"mode":"background"},"effect":"unverifiable","hint":"Read what changed","next_call":{"arguments":{"pid":1025,"since":"latest","window_id":27262979},"tool":"get_window_state"},"route":"accessibility"}),
        ] {
            check(&good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        for bad in [
            json!({"route":"accessibility"}),
            json!({"effect":"confirmed","route":"accessibility"}),
            json!({"effect":"done","route":"accessibility"}),
            json!({"effect":"confirmed","route":"teleport","evidence":[{"kind":"value_readback"}]}),
            json!({"effect":"refused","route":"virtual_hid"}),
            json!({"effect":"unverifiable","route":"virtual_hid"}),
            json!({"effect":"confirmed","route":"dom","evidence":[{"kind":"guess"}]}),
            json!({"effect":"suspected_noop","route":"dom","next_call":{"tool":"","arguments":{}}}),
            json!({"effect":"confirmed","route":"dom","evidence":[{"kind":"window_change"}],"escalation":{"target":"pixel","reason":"delivery_failed"}}),
        ] {
            assert!(check(&bad).is_err(), "accepted {bad}");
        }
    }
}
