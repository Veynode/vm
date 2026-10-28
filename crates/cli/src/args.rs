//! Docker-style argument parsing: options precede positionals, `--opt value`
//! and `--opt=value`, grouped short flags (`-dit`), and everything after the
//! first positional (or `--`) is passed through untouched when requested.
use anyhow::{bail, Result};
use std::collections::BTreeMap;

#[derive(Default, Debug)]
pub struct Parsed {
    pub flags: BTreeMap<String, Vec<String>>,
    pub positional: Vec<String>,
}

impl Parsed {
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }
    pub fn values(&self, name: &str) -> &[String] {
        self.flags.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// `spec` lists accepted options as `(long, short, takes_value)`.
/// When `stop_at_positional` is set, the first positional and everything after
/// it are positionals (the image and its command for `run`).
pub fn parse(
    args: &[String],
    spec: &[(&str, Option<char>, bool)],
    stop_at_positional: bool,
) -> Result<Parsed> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if arg == "--" {
            parsed.positional.extend(args[index..].iter().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            let Some(&(name, _, takes)) = spec.iter().find(|(l, _, _)| *l == name) else {
                bail!("unknown flag: --{name}")
            };
            let value = if takes {
                match inline {
                    Some(v) => v,
                    None => {
                        let Some(v) = args.get(index) else {
                            bail!("flag needs a value: --{name}")
                        };
                        index += 1;
                        v.clone()
                    }
                }
            } else {
                if inline.is_some() {
                    bail!("flag takes no value: --{name}")
                }
                String::new()
            };
            parsed.flags.entry(name.to_owned()).or_default().push(value);
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") {
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut position = 0;
            while position < chars.len() {
                let c = chars[position];
                position += 1;
                let Some(&(name, _, takes)) = spec.iter().find(|(_, s, _)| *s == Some(c)) else {
                    bail!("unknown flag: -{c}")
                };
                let value = if takes {
                    let rest: String = chars[position..].iter().collect();
                    position = chars.len();
                    if !rest.is_empty() {
                        rest.trim_start_matches('=').to_owned()
                    } else {
                        let Some(v) = args.get(index) else {
                            bail!("flag needs a value: -{c}")
                        };
                        index += 1;
                        v.clone()
                    }
                } else {
                    String::new()
                };
                parsed.flags.entry(name.to_owned()).or_default().push(value);
            }
            continue;
        }
        parsed.positional.push(arg.clone());
        if stop_at_positional {
            parsed.positional.extend(args[index..].iter().cloned());
            break;
        }
    }
    Ok(parsed)
}

/// `512m`, `2g`, `2048` (MiB) → MiB.
pub fn memory_mib(value: &str) -> Result<u64> {
    let lower = value.to_ascii_lowercase();
    let (digits, factor) = match lower.trim_end_matches('b').chars().last() {
        Some('g') => (&lower[..lower.trim_end_matches('b').len() - 1], 1024),
        Some('m') => (&lower[..lower.trim_end_matches('b').len() - 1], 1),
        _ => (lower.as_str(), 1),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid memory size: {value}"))?;
    Ok(n * factor)
}

/// `1920x1080` → (1920, 1080): even sides, 640x400 to 3840x2160.
pub fn desktop_size(value: &str) -> Result<(u32, u32)> {
    let bad = || {
        anyhow::anyhow!(
            "invalid --desktop-size {value}: expected WIDTHxHEIGHT, even, 640x400 to 3840x2160"
        )
    };
    let (w, h) = value.split_once(['x', 'X']).ok_or_else(bad)?;
    let (w, h): (u32, u32) = (w.parse().map_err(|_| bad())?, h.parse().map_err(|_| bad())?);
    if w % 2 != 0 || h % 2 != 0 || !(640..=3840).contains(&w) || !(400..=2160).contains(&h) {
        return Err(bad());
    }
    Ok((w, h))
}

/// `--idle-pause`: `off` or `0`, or a duration in seconds (`300`, `300s`),
/// minutes (`5m`), hours (`2h`) or days (`1d`) → seconds, 0 for off.
pub fn idle_seconds(value: &str) -> Result<u64> {
    let lower = value.to_ascii_lowercase();
    if lower == "off" {
        return Ok(0);
    }
    let (digits, factor) = match lower.chars().last() {
        Some('s') => (&lower[..lower.len() - 1], 1),
        Some('m') => (&lower[..lower.len() - 1], 60),
        Some('h') => (&lower[..lower.len() - 1], 3600),
        Some('d') => (&lower[..lower.len() - 1], 86400),
        _ => (lower.as_str(), 1),
    };
    let n: u64 = digits.parse().map_err(|_| {
        anyhow::anyhow!("invalid --idle-pause {value}: expected off or a duration like 300, 5m, 2h")
    })?;
    Ok(n * factor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_durations() {
        assert_eq!(idle_seconds("off").unwrap(), 0);
        assert_eq!(idle_seconds("0").unwrap(), 0);
        assert_eq!(idle_seconds("300").unwrap(), 300);
        assert_eq!(idle_seconds("90s").unwrap(), 90);
        assert_eq!(idle_seconds("5m").unwrap(), 300);
        assert_eq!(idle_seconds("2H").unwrap(), 7200);
        assert_eq!(idle_seconds("1d").unwrap(), 86400);
        for bad in ["", "m", "5x", "-1", "five"] {
            assert!(idle_seconds(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn desktop_size_is_bounded_and_even() {
        assert_eq!(desktop_size("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(desktop_size("1280X800").unwrap(), (1280, 800));
        for bad in ["1920", "1921x1080", "100x100", "5000x2000", "ax1", "1920x"] {
            assert!(desktop_size(bad).is_err(), "{bad}");
        }
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    const SPEC: &[(&str, Option<char>, bool)] = &[
        ("detach", Some('d'), false),
        ("interactive", Some('i'), false),
        ("tty", Some('t'), false),
        ("publish", Some('p'), true),
        ("env", Some('e'), true),
        ("rm", None, false),
        ("name", None, true),
    ];
    #[test]
    fn docker_style() {
        let p = parse(
            &s(&[
                "-dit",
                "-p8080:80",
                "-p",
                "9000:9000",
                "--rm",
                "--name=web",
                "-e",
                "A=1",
                "nginx",
                "-g",
                "daemon off;",
            ]),
            SPEC,
            true,
        )
        .unwrap();
        assert!(p.has("detach") && p.has("interactive") && p.has("tty") && p.has("rm"));
        assert_eq!(p.values("publish"), ["8080:80", "9000:9000"]);
        assert_eq!(p.value("name"), Some("web"));
        assert_eq!(p.positional, ["nginx", "-g", "daemon off;"]);
        assert!(parse(&s(&["--bogus"]), SPEC, true).is_err());
    }
    #[test]
    fn memory_units() {
        assert_eq!(memory_mib("512m").unwrap(), 512);
        assert_eq!(memory_mib("2g").unwrap(), 2048);
        assert_eq!(memory_mib("2GB").unwrap(), 2048);
        assert_eq!(memory_mib("1024").unwrap(), 1024);
        assert!(memory_mib("x").is_err());
    }
}
