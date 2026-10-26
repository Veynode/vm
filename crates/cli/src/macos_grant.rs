//! Grant the macOS Guest's SSH sessions Accessibility and Automation of
//! System Events on first boot, so commands run with `exec` (AppleScript,
//! `osascript`, UI tools) can drive the desktop without anyone opening System
//! Settings.
//!
//! SIP keeps the system TCC database read-only even for root, so the grant
//! goes through the same UI a person would use. Asking `AXIsProcessTrusted`
//! over SSH adds `sshd-keygen-wrapper` (the responsible process for SSH
//! sessions) to Privacy & Security › Accessibility, switched off. The host
//! opens that pane, finds the row with text recognition on a daemon screenshot,
//! finds the switch at the end of the row by scanning its pixels, clicks it,
//! types the login password into the authorization dialog and confirms that
//! SSH sessions are now trusted. Then it sends System Events an Apple event
//! over SSH and clicks Allow on the consent prompt that follows. Clicks and
//! keys go through the daemon's input path, like any other caller.
use crate::{
    container::Container,
    daemon_call,
    macos::{login, remote},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use vm_control::screen_text::vision;

const TRUSTED: &str =
    "/usr/bin/osascript -l JavaScript -e 'ObjC.import(\"ApplicationServices\"); $.AXIsProcessTrusted()'";
const PANE: &str =
    "/usr/bin/open 'x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility'";
const QUIT: &str = "/usr/bin/killall 'System Settings'";
const ROW: &str = "sshd-keygen-wrapper";
const DIALOG: &str = "Modify Settings";
const EVENTS: &str = "/usr/bin/osascript -e 'with timeout of 60 seconds' -e 'tell application \"System Events\" to count processes' -e 'end timeout'";
const ALLOW: &str = "Allow";
// Input needs a frame anchor younger than five seconds.
const ANCHOR_AGE: Duration = Duration::from_secs(3);

/// Whether SSH sessions in the Guest are trusted for Accessibility. Asking
/// also lists them in the Accessibility pane.
fn trusted(container: &Container, ip: &str) -> Result<bool> {
    let (code, out, err) = remote(container, ip, TRUSTED, None)?;
    if code != 0 {
        bail!(
            "Accessibility check failed: {}",
            String::from_utf8_lossy(&err).trim()
        )
    }
    Ok(String::from_utf8_lossy(&out).trim() == "true")
}

/// The physical keys (US layout) that type `c`, as one chord.
fn chord(c: char) -> Option<Vec<&'static str>> {
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    let plain = |key| Some(vec![key]);
    let shifted = |key| Some(vec!["shift", key]);
    match c {
        'a'..='z' => plain(LETTERS[c as usize - 'a' as usize]),
        'A'..='Z' => shifted(LETTERS[c as usize - 'A' as usize]),
        '0'..='9' => plain(DIGITS[c as usize - '0' as usize]),
        ' ' => plain("spc"),
        '-' => plain("minus"),
        '=' => plain("equal"),
        '[' => plain("bracket_left"),
        ']' => plain("bracket_right"),
        ';' => plain("semicolon"),
        '\'' => plain("apostrophe"),
        '`' => plain("grave_accent"),
        '\\' => plain("backslash"),
        ',' => plain("comma"),
        '.' => plain("dot"),
        '/' => plain("slash"),
        ')' => shifted("0"),
        '!' => shifted("1"),
        '@' => shifted("2"),
        '#' => shifted("3"),
        '$' => shifted("4"),
        '%' => shifted("5"),
        '^' => shifted("6"),
        '&' => shifted("7"),
        '*' => shifted("8"),
        '(' => shifted("9"),
        '_' => shifted("minus"),
        '+' => shifted("equal"),
        '{' => shifted("bracket_left"),
        '}' => shifted("bracket_right"),
        ':' => shifted("semicolon"),
        '"' => shifted("apostrophe"),
        '~' => shifted("grave_accent"),
        '|' => shifted("backslash"),
        '<' => shifted("comma"),
        '>' => shifted("dot"),
        '?' => shifted("slash"),
        _ => None,
    }
}

struct Shot {
    reply: Value,
    taken: Instant,
    lines: std::cell::OnceCell<Value>,
}

impl Shot {
    fn take(vm: &str) -> Result<Shot> {
        Ok(Shot {
            reply: daemon_call("screenshot", json!({"id":vm}))?,
            taken: Instant::now(),
            lines: Default::default(),
        })
    }
    fn input(&self, vm: &str, method: &str, mut args: Value) -> Result<()> {
        for key in ["frame_id", "source_generation", "width", "height"] {
            args[key] = self.reply[key].clone();
        }
        args["id"] = json!(vm);
        daemon_call(method, args).map(|_| ())
    }
    /// The first line on screen that reads `text` and nothing else (so
    /// "Allow" is not "Don't Allow"). Recognizes the screenshot once.
    fn find(&self, text: &str) -> Result<Option<Value>> {
        if self.lines.get().is_none() {
            let mut found = vision(&["text".into()], &self.reply)?;
            let _ = self.lines.set(found["lines"].take());
        }
        Ok(self.lines.get().unwrap().as_array().and_then(|lines| {
            lines
                .iter()
                .find(|l| {
                    l["text"]
                        .as_str()
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case(text))
                })
                .cloned()
        }))
    }
}

/// Takes screenshots until `text` shows up in the same place twice in a row,
/// so the window or sheet carrying it has stopped moving.
fn wait_for(vm: &str, text: &str, timeout: Duration) -> Result<(Shot, Value)> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    let mut failure = None;
    loop {
        let shot = Shot::take(vm)?;
        // Vision now and then fails on a frame (CRImageReaderError): poll on.
        let found = shot.find(text).unwrap_or_else(|e| {
            failure = Some(e);
            None
        });
        let place = match &found {
            Some(found) => Some((number(found, "x")?, number(found, "y")?)),
            None => None,
        };
        if place.is_some() && place == last {
            return Ok((shot, found.unwrap()));
        }
        if Instant::now() > deadline {
            match failure {
                Some(e) => bail!("\"{text}\" did not appear on the Guest's screen (last recognition error: {e:#})"),
                None => bail!("\"{text}\" did not appear on the Guest's screen"),
            }
        }
        last = place;
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn number(value: &Value, key: &str) -> Result<i64> {
    value[key]
        .as_i64()
        .with_context(|| format!("text recognition result missing {key}"))
}

/// The switch at the end of the row that holds `label`.
fn switch_on_row(shot: &Shot, label: &Value) -> Result<Option<(i64, i64)>> {
    let y = number(label, "center_y")?;
    let from = number(label, "x")? + number(label, "width")? + 8;
    let scan = vision(
        &[
            "row".into(),
            "--y".into(),
            y.to_string(),
            "--from".into(),
            from.to_string(),
        ],
        &shot.reply,
    )?;
    let switch = scan["runs"].as_array().and_then(|runs| {
        runs.iter()
            .find(|r| r["width"].as_i64().is_some_and(|w| (4..=70).contains(&w)))
    });
    Ok(match switch {
        Some(switch) => Some((number(switch, "center_x")?, y)),
        None => None,
    })
}

fn wait_trusted(container: &Container, ip: &str, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if trusted(container, ip)? {
            return Ok(true);
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Grants both permissions, warning about each one that could not be granted.
pub fn grant(container: &Container, ip: &str) {
    if let Err(e) = accessibility(container, ip) {
        eprintln!("warning: could not grant Accessibility to SSH sessions: {e:#}");
    }
    if let Err(e) = automation(container, ip) {
        eprintln!("warning: could not let SSH sessions script System Events: {e:#}");
    }
}

fn accessibility(container: &Container, ip: &str) -> Result<()> {
    if trusted(container, ip)? {
        return Ok(());
    }
    let (_, password) = login(container)?;
    let keys = password
        .chars()
        .map(chord)
        .chain(std::iter::once(Some(vec!["ret"])))
        .collect::<Option<Vec<_>>>()
        .context("the login password has characters that cannot be typed on a US keyboard")?;
    let vm = container.vm_id.as_str();
    let result = (|| -> Result<()> {
        // The pane lists its rows when it opens: on a busy first boot it can
        // open before the row exists, or take long to open at all.
        let mut opened = Err(anyhow::anyhow!("not opened"));
        for _ in 0..3 {
            let _ = remote(container, ip, QUIT, None);
            remote(container, ip, PANE, None)?;
            // A cold text recognizer can take tens of seconds on its first run.
            opened = wait_for(vm, ROW, Duration::from_secs(40));
            if opened.is_ok() {
                break;
            }
        }
        opened?;
        // On a first boot the pane can still be loading when the row shows
        // up, so a click can go nowhere, and the dialog can drop keys typed
        // as it opens: each attempt starts over from the row.
        for _ in 0..3 {
            let (shot, row) = wait_for(vm, ROW, Duration::from_secs(40))?;
            // The dialog can be up already, from an earlier click.
            if shot.find(DIALOG)?.is_none() {
                let Some((x, y)) = switch_on_row(&shot, &row)? else {
                    continue;
                };
                let shot = if shot.taken.elapsed() > ANCHOR_AGE {
                    Shot::take(vm)?
                } else {
                    shot
                };
                shot.input(vm, "input_click", json!({"x":x,"y":y}))?;
            }
            let Ok((mut shot, _)) = wait_for(vm, DIALOG, Duration::from_secs(10)) else {
                continue;
            };
            for chord in &keys {
                if shot.taken.elapsed() > ANCHOR_AGE {
                    shot = Shot::take(vm)?;
                }
                shot.input(vm, "input_key", json!({"keys":chord}))?;
            }
            if wait_trusted(container, ip, Duration::from_secs(10))? {
                return Ok(());
            }
            // A rejected password leaves the dialog up: cancel it.
            let shot = Shot::take(vm)?;
            if shot.find(DIALOG)?.is_some() {
                shot.input(vm, "input_key", json!({"keys":["esc"]}))?;
            }
        }
        bail!("the Accessibility switch did not turn on")
    })();
    let _ = remote(container, ip, QUIT, None);
    result
}

/// The first Apple event to System Events (which scripts the UI of every
/// app) asks for consent with an Allow button; no password.
fn automation(container: &Container, ip: &str) -> Result<()> {
    let vm = container.vm_id.as_str();
    std::thread::scope(|scope| {
        let events = scope.spawn(|| remote(container, ip, EVENTS, None));
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = None;
        while !events.is_finished() {
            let shot = Shot::take(vm)?;
            let place = match shot.find(ALLOW).unwrap_or(None) {
                Some(allow) => Some((number(&allow, "center_x")?, number(&allow, "center_y")?)),
                None => None,
            };
            if let Some((x, y)) = place.filter(|_| place == last) {
                shot.input(vm, "input_click", json!({"x":x,"y":y}))?;
                last = None;
            } else {
                last = place;
            }
            if Instant::now() > deadline {
                bail!("no consent prompt appeared")
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let (code, _, err) = events.join().unwrap()?;
        if code != 0 {
            bail!("{}", String::from_utf8_lossy(&err).trim())
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn password_characters_map_to_us_chords() {
        let keys = vm_control::input::key_names();
        for c in (' '..='~').filter(|c| *c != '\x7f') {
            let chord = chord(c).unwrap_or_else(|| panic!("{c:?} has no chord"));
            assert!(chord.iter().all(|k| keys.iter().any(|n| n == k)), "{c:?}");
        }
        assert_eq!(chord('A').unwrap(), ["shift", "a"]);
        assert_eq!(chord('?').unwrap(), ["shift", "slash"]);
        assert_eq!(chord('7').unwrap(), ["7"]);
        assert!(chord('é').is_none() && chord('\n').is_none());
    }
}
