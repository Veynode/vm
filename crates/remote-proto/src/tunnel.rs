//! Gateway to Mac host tunnel (QUIC, ALPN [`ALPN`], mutual TLS: the
//! gateway pins each host's certificate key, the host pins the gateway's).
//!
//! The gateway never parses what it forwards beyond this framing:
//! - One bidirectional stream per browser session, opened by the gateway.
//!   It starts with a [`RouteOpen`] frame, then carries the session's
//!   reliable messages both ways as `u32 len | message` frames.
//! - Video, host to browser: a unidirectional stream per access unit,
//!   opened by the host, `u64 route_id` then one message until FIN. A host
//!   reset (deadline passed, superseded) is propagated to the browser stream.
//! - Datagrams both ways: `u64 route_id | message`.
//!
//! A "message" is what the browser sees: a handshake record
//! (`u8 kind = AUTH | JSON`) before keys, a sealed record
//! (`u8 kind | u64 seq | ciphertext`) after. Messages whose first byte is
//! [`GATEWAY_NOTICE`] are written by the gateway itself (residence times,
//! closing reasons) and never reach the host.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"veynode-tunnel/1";
/// First byte of a gateway-to-browser message the gateway writes itself.
pub const GATEWAY_NOTICE: u8 = 0xff;
/// Largest message: a 4 MiB access unit plus record and video headers.
pub const MAX_MESSAGE: usize = crate::record::MAX_PAYLOAD + 64;

/// First frame of a route stream: who the browser is, as the gateway saw it.
/// `transport` and `origin` are bound into the signed offer, so a gateway
/// that lies here only makes the handshake fail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteOpen {
    pub route_id: u64,
    /// `wt` or `wss`.
    pub transport: String,
    pub origin: String,
    /// The browser's address, for the host's rate limits and logs.
    pub remote: String,
}

pub fn frame(message: &[u8]) -> Result<Vec<u8>> {
    ensure!(message.len() <= MAX_MESSAGE, "message too large");
    let mut out = Vec::with_capacity(4 + message.len());
    out.extend((message.len() as u32).to_be_bytes());
    out.extend(message);
    Ok(out)
}

/// Length of the frame starting `header`, validated.
pub fn frame_len(header: [u8; 4]) -> Result<usize> {
    let n = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_MESSAGE).contains(&n),
        "invalid tunnel frame length {n}"
    );
    Ok(n)
}

pub fn routed(route_id: u64, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + message.len());
    out.extend(route_id.to_be_bytes());
    out.extend(message);
    out
}

pub fn unroute(bytes: &[u8]) -> Result<(u64, &[u8])> {
    ensure!(bytes.len() > 8, "short routed message");
    Ok((u64::from_be_bytes(bytes[..8].try_into()?), &bytes[8..]))
}

/// A gateway notice: `0xff | JSON`.
pub fn notice(value: &serde_json::Value) -> Vec<u8> {
    let mut out = vec![GATEWAY_NOTICE];
    out.extend(value.to_string().into_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_routes() {
        let f = frame(b"\x07{}").unwrap();
        assert_eq!(frame_len(f[..4].try_into().unwrap()).unwrap(), 3);
        assert!(frame_len([0; 4]).is_err());
        assert!(frame_len(u32::MAX.to_be_bytes()).is_err());
        let r = routed(42, b"x");
        assert_eq!(unroute(&r).unwrap(), (42, &b"x"[..]));
        assert!(unroute(&r[..8]).is_err());
        assert_eq!(notice(&serde_json::json!({"gw":1}))[0], GATEWAY_NOTICE);
    }

    #[test]
    fn route_open_rejects_unknown_fields() {
        let o: RouteOpen = serde_json::from_str(
            r#"{"route_id":1,"transport":"wt","origin":"https://a.example","remote":"1.2.3.4:5"}"#,
        )
        .unwrap();
        assert_eq!(o.route_id, 1);
        assert!(serde_json::from_str::<RouteOpen>(
            r#"{"route_id":1,"transport":"wt","origin":"o","remote":"r","x":1}"#
        )
        .is_err());
    }
}
