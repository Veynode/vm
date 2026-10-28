//! Small pieces the services share.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    ffi::CString,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use veynodevm_api::{refuse, ErrorCode};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let mut file = fs::File::open("/dev/urandom").expect("/dev/urandom");
    std::io::Read::read_exact(&mut file, &mut buffer).expect("random bytes");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// Random locally administered unicast MAC.
pub fn random_mac() -> String {
    let hex = random_hex(6);
    let first = u8::from_str_radix(&hex[..2], 16).unwrap() & 0xfc | 0x02;
    let rest: Vec<&str> = (1..6).map(|i| &hex[2 * i..2 * i + 2]).collect();
    format!("{first:02x}:{}", rest.join(":"))
}

fn normalize_mac(mac: &str) -> Option<String> {
    let octets: Vec<u8> = mac
        .split(':')
        .map(|o| u8::from_str_radix(o, 16).ok())
        .collect::<Option<_>>()?;
    (octets.len() == 6).then(|| {
        octets
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// vmnet's DHCP leases on the host.
pub const LEASES: &str = "/var/db/dhcpd_leases";

/// Newest lease for `mac` in a vmnet `dhcpd_leases` file. Its hardware
/// addresses drop leading zeros ("1,a:2:..."), so both sides are normalized.
pub fn lease_ip(leases: &str, mac: &str) -> Option<String> {
    let wanted = normalize_mac(mac)?;
    let mut best: Option<(u64, String)> = None;
    for block in leases.split('}') {
        let mut ip = None;
        let mut hw = None;
        let mut lease = 0;
        for line in block.lines() {
            match line.trim().split_once('=') {
                Some(("ip_address", v)) => ip = Some(v.to_owned()),
                Some(("hw_address", v)) => {
                    hw = v.split_once(',').and_then(|(_, m)| normalize_mac(m))
                }
                Some(("lease", v)) => {
                    lease = u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0)
                }
                _ => {}
            }
        }
        if let (Some(ip), Some(hw)) = (ip, hw) {
            if hw == wanted && best.as_ref().is_none_or(|(l, _)| lease >= *l) {
                best = Some((lease, ip));
            }
        }
    }
    best.map(|(_, ip)| ip)
}

/// The current lease for `mac`, if vmnet has given it one.
pub fn leased_ip(mac: &str) -> Option<String> {
    fs::read_to_string(LEASES)
        .ok()
        .and_then(|leases| lease_ip(&leases, mac))
}

/// An APFS copy-on-write clone of `source` at `target`, private to this user.
pub fn clone_file(source: &Path, target: &Path) -> Result<()> {
    let s = CString::new(source.as_os_str().as_encoded_bytes())?;
    let t = CString::new(target.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::clonefile(s.as_ptr(), t.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("APFS clone of {} failed", source.display()));
    }
    // Image files are read-only; the clone is its owner's writable copy.
    fs::set_permissions(target, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub const ICON_KINDS: [&str; 10] = [
    "monitor", "laptop", "vm", "box", "server", "terminal", "cpu", "globe", "folder", "code",
];
pub const ICON_COLORS: [&str; 8] = [
    "blue", "teal", "green", "yellow", "orange", "pink", "purple", "black",
];

/// Checks an icon, `KIND` (the kind's own colour) or `KIND:COLOR`.
pub fn parse_icon(icon: &str) -> Result<String> {
    let (kind, color) = match icon.split_once(':') {
        Some((kind, color)) => (kind, Some(color)),
        None => (icon, None),
    };
    if !ICON_KINDS.contains(&kind) {
        return Err(refuse(
            ErrorCode::Invalid,
            format!(
                "unknown icon {kind:?}; use one of {}",
                ICON_KINDS.join(", ")
            ),
        ));
    }
    if let Some(color) = color.filter(|c| !ICON_COLORS.contains(c)) {
        return Err(refuse(
            ErrorCode::Invalid,
            format!(
                "unknown icon colour {color:?}; use one of {}",
                ICON_COLORS.join(", ")
            ),
        ));
    }
    Ok(icon.to_owned())
}

/// A required string argument.
pub fn string<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    match &args[field] {
        Value::String(s) => Ok(s),
        Value::Null => Err(refuse(ErrorCode::Invalid, format!("{field} is required"))),
        _ => Err(refuse(
            ErrorCode::Invalid,
            format!("{field} must be a string"),
        )),
    }
}

/// An optional string argument.
pub fn opt_string<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match &args[field] {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        _ => Err(refuse(
            ErrorCode::Invalid,
            format!("{field} must be a string"),
        )),
    }
}

/// An optional unsigned integer argument.
pub fn opt_u64(args: &Value, field: &str) -> Result<Option<u64>> {
    match &args[field] {
        Value::Null => Ok(None),
        value => value.as_u64().map(Some).ok_or_else(|| {
            refuse(
                ErrorCode::Invalid,
                format!("{field} must be a whole number"),
            )
        }),
    }
}

/// An optional boolean argument, `false` when absent.
pub fn flag(args: &Value, field: &str) -> Result<bool> {
    match &args[field] {
        Value::Null => Ok(false),
        Value::Bool(b) => Ok(*b),
        _ => Err(refuse(
            ErrorCode::Invalid,
            format!("{field} must be true or false"),
        )),
    }
}

/// Refuses arguments this method does not take, so a misspelt one is not
/// silently ignored.
pub fn only(args: &Value, allowed: &[&str]) -> Result<()> {
    let Some(fields) = args.as_object() else {
        bail!("arguments must be an object")
    };
    if let Some(field) = fields.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(refuse(
            ErrorCode::Invalid,
            format!("unknown argument {field}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_lookup_normalizes_and_prefers_newest() {
        let leases = "{\n\tname=old\n\tip_address=192.168.64.3\n\thw_address=1,72:6:f4:5e:d8:14\n\tlease=0x10\n}\n{\n\tname=new\n\tip_address=192.168.64.9\n\thw_address=1,72:6:f4:5e:d8:14\n\tlease=0x20\n}\n{\n\tip_address=192.168.64.4\n\thw_address=1,2:0:0:0:0:1\n\tlease=0x30\n}\n";
        assert_eq!(
            lease_ip(leases, "72:06:f4:5e:d8:14").as_deref(),
            Some("192.168.64.9")
        );
        assert_eq!(lease_ip(leases, "02:00:00:00:00:02"), None);
    }

    #[test]
    fn macs_are_local_unicast() {
        for _ in 0..32 {
            let mac = random_mac();
            let first = u8::from_str_radix(&mac[..2], 16).unwrap();
            assert_eq!(first & 0b11, 0b10, "{mac}");
            assert_eq!(normalize_mac(&mac).unwrap(), mac);
        }
    }

    #[test]
    fn icons_are_checked() {
        assert!(parse_icon("vm").is_ok() && parse_icon("vm:teal").is_ok());
        assert!(parse_icon("ufo").is_err() && parse_icon("vm:plaid").is_err());
    }
}
