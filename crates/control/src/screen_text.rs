//! `vm_screen_text`: read the text on a VM screen. Takes a screenshot through
//! the daemon (any backend that has one) and runs the host `vm-vision` helper
//! (`native/tools/vm-vision.swift`, Apple Vision) on it. Boxes come back in
//! screenshot pixels with the screenshot's frame anchor, so a caller can click
//! a recognized word with `vm_input_click` directly.
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

// The first run on a host loads Vision's models, which can take tens of seconds.
const HELPER_DEADLINE: Duration = Duration::from_secs(60);
const MAX_HELPER_OUTPUT: usize = 4 * 1024 * 1024;
// Leaves room in the 1 MiB MCP message for the envelope.
const MAX_REPLY: usize = 900 * 1024;

pub fn arguments(args: &Value) -> Result<Vec<String>> {
    let mut flags = Vec::new();
    if args.get("fast").is_some_and(|v| v == true) {
        flags.push("--fast".into());
    }
    if let Some(languages) = args.get("languages") {
        let list = languages
            .as_array()
            .filter(|l| l.len() <= 8)
            .context("languages must be at most 8 BCP 47 codes")?;
        for language in list {
            let code = language
                .as_str()
                .filter(|c| {
                    (2..=16).contains(&c.len())
                        && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .context("invalid language code")?;
            flags.extend(["--language".into(), code.into()]);
        }
    }
    if let Some(find) = args.get("find") {
        let text = find
            .as_str()
            .filter(|t| !t.is_empty() && t.chars().count() <= 256 && !t.contains('\0'))
            .context("find must be 1..256 characters")?;
        flags.extend(["--find".into(), text.into()]);
    }
    Ok(flags)
}

fn helper() -> Result<PathBuf> {
    if let Some(path) = crate::home::Runtime::load()?.vision {
        return Ok(path);
    }
    let sibling = std::env::current_exe()?.with_file_name("vm-vision");
    if sibling.is_file() {
        return Ok(sibling);
    }
    bail!("unsupported: text recognition helper not registered (build native/tools/build-vm-vision.sh, then scripts/install-dev-runtime.py)")
}

/// Runs one `vm-vision` mode (`text`, `row`) on a daemon `screenshot` reply,
/// for host code that already holds one.
pub fn vision(args: &[String], shot: &Value) -> Result<Value> {
    run_helper(&helper()?, args, decode(shot)?)
}

/// Runs one `vm-vision` mode on image bytes.
pub fn vision_image(args: &[String], image: Vec<u8>) -> Result<Value> {
    run_helper(&helper()?, args, image)
}

pub(crate) fn decode(shot: &Value) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(
            shot["data_base64"]
                .as_str()
                .context("screenshot missing image")?,
        )
        .context("invalid screenshot base64")
}

fn recognize(helper: &Path, flags: &[String], image: Vec<u8>) -> Result<Value> {
    let args: Vec<String> = std::iter::once("text".to_string())
        .chain(flags.iter().cloned())
        .collect();
    run_helper(helper, &args, image)
}

fn run_helper(helper: &Path, args: &[String], image: Vec<u8>) -> Result<Value> {
    let mut child = Command::new(helper)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", helper.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || stdin.write_all(&image));
    let mut stdout = child
        .stdout
        .take()
        .unwrap()
        .take(MAX_HELPER_OUTPUT as u64 + 1);
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).map(|_| out)
    });
    let mut stderr = child.stderr.take().unwrap().take(4096);
    let errors = thread::spawn(move || {
        let mut out = String::new();
        let _ = stderr.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + HELPER_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("text recognition exceeded {}s", HELPER_DEADLINE.as_secs())
        }
        thread::sleep(Duration::from_millis(20));
    };
    let _ = writer.join();
    let out = reader.join().unwrap()?;
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        bail!("text recognition failed: {}", errors.trim())
    }
    if out.len() > MAX_HELPER_OUTPUT {
        bail!("text recognition output exceeds bound")
    }
    serde_json::from_slice(&out).context("invalid text recognition output")
}

pub fn run(dir: &Path, args: &Value) -> Result<Value> {
    let flags = arguments(args)?;
    let helper = helper()?;
    let shot = crate::call(dir, "screenshot", json!({"id":args["id"]}))?;
    let text = recognize(&helper, &flags, decode(&shot)?)?;
    result(&shot, text)
}

/// The `vm_screen_text` reply for a helper's `text` output on `shot`.
pub fn result(shot: &Value, mut text: Value) -> Result<Value> {
    if text["width"] != shot["width"] || text["height"] != shot["height"] {
        bail!("recognized image size differs from the screenshot")
    }
    let mut result = json!({
        "frame_id": shot["frame_id"],
        "source_generation": shot["source_generation"],
        "width": shot["width"],
        "height": shot["height"],
        "coordinate_space": "screenshot-pixels",
        "recognizer": text["recognizer"],
        "lines": text["lines"].take(),
    });
    if let Some(matches) = text.get_mut("matches") {
        result["matches"] = matches.take();
    }
    // A screen full of small text can exceed the MCP message; keep the
    // matches and as many lines as fit, in reading order.
    if serde_json::to_vec(&result)?.len() > MAX_REPLY {
        let lines = result["lines"].as_array_mut().unwrap();
        let total = lines.len();
        while !lines.is_empty() && serde_json::to_vec(&lines)?.len() > MAX_REPLY / 2 {
            lines.truncate(lines.len() * 3 / 4);
        }
        result["lines_truncated_from"] = json!(total);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_become_bounded_helper_flags() {
        assert_eq!(
            arguments(
                &json!({"id":"vm","fast":true,"languages":["en-US","zh-Hans"],"find":"Save"})
            )
            .unwrap(),
            [
                "--fast",
                "--language",
                "en-US",
                "--language",
                "zh-Hans",
                "--find",
                "Save"
            ]
        );
        assert!(arguments(&json!({"id":"vm"})).unwrap().is_empty());
        for bad in [
            json!({"languages":["en US"]}),
            json!({"languages":"en"}),
            json!({"find":""}),
            json!({"find":"x".repeat(257)}),
        ] {
            assert!(arguments(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn helper_output_is_parsed_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("vm-vision");
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat >/dev/null\necho '{\"width\":4,\"height\":2,\"lines\":[{\"text\":\"ok\"}],\"recognizer\":\"fake\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let out = recognize(&fake, &[], vec![0; 16]).unwrap();
        assert_eq!(out["lines"][0]["text"], "ok");
        std::fs::write(&fake, "#!/bin/sh\necho broken >&2\nexit 2\n").unwrap();
        let error = recognize(&fake, &[], vec![]).unwrap_err().to_string();
        assert!(error.contains("broken"), "{error}");
    }
}
