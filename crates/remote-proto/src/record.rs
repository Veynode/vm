//! Plaintext records: `u32 len | u8 kind | payload`, `len = payload + 1`,
//! big-endian, the same framing `veynodevm desktop --stream` writes. Kinds 1
//! and 2 keep their local meaning; the rest are remote additions.
use anyhow::{bail, ensure, Context, Result};

/// JSON event, host to browser.
pub const EVENT: u8 = 1;
/// Video v1: `u64 pts_us, u8 keyframe, Annex B`.
pub const VIDEO: u8 = 2;
/// Input JSON (`EventWire` events plus `seq`), browser to host.
pub const INPUT: u8 = 3;
/// Control JSON (`keyframe_req`, `rate`, `takeover`, `release`, `activity`, `subscribe`).
pub const CONTROL: u8 = 4;
/// Ping or pong JSON.
pub const PING: u8 = 5;
/// Receiver statistics JSON, browser to host, once a second.
pub const STATS: u8 = 6;
/// Handshake JSON (`hello`, `challenge`, `offer`, `answer`, `confirm`).
pub const AUTH: u8 = 7;
/// Video v2: [`VideoHeader`] then Annex B.
pub const VIDEO_V2: u8 = 8;
/// Latest pointer position, browser to host: [`Pointer`].
pub const POINTER: u8 = 9;

/// Largest payload accepted: the 4 MiB access-unit ceiling of the local
/// encoders (`frame_schedule::frame_header`) plus headers.
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024 + 64;
/// Largest JSON payload (input, control, auth, stats).
pub const MAX_JSON: usize = 64 * 1024;

pub fn known(kind: u8) -> bool {
    (EVENT..=POINTER).contains(&kind)
}

pub fn encode(kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(known(kind), "unknown record kind {kind}");
    ensure!(payload.len() <= MAX_PAYLOAD, "record payload too large");
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend(((payload.len() + 1) as u32).to_be_bytes());
    out.push(kind);
    out.extend(payload);
    Ok(out)
}

/// One record from the front of `bytes`: `(kind, payload, consumed)`, or
/// `None` while the record is incomplete.
pub fn decode(bytes: &[u8]) -> Result<Option<(u8, &[u8], usize)>> {
    if bytes.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
    ensure!(
        len >= 1 && len - 1 <= MAX_PAYLOAD,
        "invalid record length {len}"
    );
    if bytes.len() < 4 + len {
        return Ok(None);
    }
    let kind = bytes[4];
    ensure!(known(kind), "unknown record kind {kind}");
    let payload = &bytes[5..4 + len];
    if kind != VIDEO && kind != VIDEO_V2 && kind != POINTER {
        ensure!(payload.len() <= MAX_JSON, "JSON record too large");
    }
    Ok(Some((kind, payload, 4 + len)))
}

/// Header of a [`VIDEO_V2`] payload. Times are host-clock microseconds
/// relative to `pts_us`, so no clock leaves the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoHeader {
    pub epoch: u32,
    pub frame_seq: u32,
    pub keyframe: bool,
    pub sps_changed: bool,
    pub pts_us: u64,
    /// Encode completion minus capture (PTS).
    pub encode_us: u32,
    /// Time the frame waited in the host pacer.
    pub host_queue_us: u32,
    /// Highest input `seq` the host had injected when this frame was captured.
    pub input_ack_seq: u32,
    /// The picture this frame shows: a rectangle of the source screen.
    pub view: View,
}

/// A rectangle `x, y, w, h` of a `src_w` by `src_h` source screen, in source
/// pixels. The whole screen when `x = y = 0, w = src_w, h = src_h`. The
/// browser maps pointer positions through it back to source coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub src_w: u16,
    pub src_h: u16,
}

impl View {
    pub const LEN: usize = 12;

    pub fn full(src_w: u16, src_h: u16) -> Self {
        Self {
            x: 0,
            y: 0,
            w: src_w,
            h: src_h,
            src_w,
            src_h,
        }
    }

    pub fn is_full(&self) -> bool {
        self.x == 0 && self.y == 0 && self.w == self.src_w && self.h == self.src_h
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.src_w >= 2 && self.src_h >= 2, "empty source");
        ensure!(self.w >= 2 && self.h >= 2, "empty view");
        ensure!(
            u32::from(self.x) + u32::from(self.w) <= u32::from(self.src_w)
                && u32::from(self.y) + u32::from(self.h) <= u32::from(self.src_h),
            "view outside the source"
        );
        Ok(())
    }

    fn put(&self, out: &mut Vec<u8>) {
        for v in [self.x, self.y, self.w, self.h, self.src_w, self.src_h] {
            out.extend(v.to_be_bytes());
        }
    }

    fn get(b: &[u8]) -> Self {
        let at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        Self {
            x: at(0),
            y: at(2),
            w: at(4),
            h: at(6),
            src_w: at(8),
            src_h: at(10),
        }
    }
}

impl VideoHeader {
    pub const LEN: usize = 4 + 4 + 1 + 8 + 4 + 4 + 4 + View::LEN;
    const KEY: u8 = 1;
    const SPS: u8 = 2;

    pub fn encode(&self, annex_b: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN + annex_b.len());
        out.extend(self.epoch.to_be_bytes());
        out.extend(self.frame_seq.to_be_bytes());
        out.push(
            if self.keyframe { Self::KEY } else { 0 }
                | if self.sps_changed { Self::SPS } else { 0 },
        );
        out.extend(self.pts_us.to_be_bytes());
        out.extend(self.encode_us.to_be_bytes());
        out.extend(self.host_queue_us.to_be_bytes());
        out.extend(self.input_ack_seq.to_be_bytes());
        self.view.put(&mut out);
        out.extend(annex_b);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<(Self, &[u8])> {
        ensure!(payload.len() > Self::LEN, "short video_v2 payload");
        let u32_at = |i: usize| u32::from_be_bytes(payload[i..i + 4].try_into().unwrap());
        let flags = payload[8];
        ensure!(flags & !(Self::KEY | Self::SPS) == 0, "unknown video flags");
        let view = View::get(&payload[29..29 + View::LEN]);
        view.validate()?;
        Ok((
            Self {
                epoch: u32_at(0),
                frame_seq: u32_at(4),
                keyframe: flags & Self::KEY != 0,
                sps_changed: flags & Self::SPS != 0,
                pts_us: u64::from_be_bytes(payload[9..17].try_into()?),
                encode_us: u32_at(17),
                host_queue_us: u32_at(21),
                input_ack_seq: u32_at(25),
                view,
            },
            &payload[Self::LEN..],
        ))
    }
}

/// [`VIDEO`] v1 payload, as the local relay writes it.
pub fn encode_video_v1(pts_us: u64, keyframe: bool, annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + annex_b.len());
    out.extend(pts_us.to_be_bytes());
    out.push(keyframe as u8);
    out.extend(annex_b);
    out
}

pub fn decode_video_v1(payload: &[u8]) -> Result<(u64, bool, &[u8])> {
    ensure!(payload.len() > 9, "short video payload");
    let keyframe = match payload[8] {
        0 => false,
        1 => true,
        n => bail!("invalid keyframe flag {n}"),
    };
    Ok((
        u64::from_be_bytes(payload[..8].try_into()?),
        keyframe,
        &payload[9..],
    ))
}

/// [`POINTER`] payload: the newest absolute position in a `w` by `h` view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub seq: u32,
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}

impl Pointer {
    pub const LEN: usize = 12;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..4].copy_from_slice(&self.seq.to_be_bytes());
        out[4..6].copy_from_slice(&self.x.to_be_bytes());
        out[6..8].copy_from_slice(&self.y.to_be_bytes());
        out[8..10].copy_from_slice(&self.w.to_be_bytes());
        out[10..12].copy_from_slice(&self.h.to_be_bytes());
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let b: &[u8; Self::LEN] = payload.try_into().context("pointer payload length")?;
        let p = Self {
            seq: u32::from_be_bytes(b[..4].try_into()?),
            x: i16::from_be_bytes([b[4], b[5]]),
            y: i16::from_be_bytes([b[6], b[7]]),
            w: u16::from_be_bytes([b[8], b[9]]),
            h: u16::from_be_bytes([b[10], b[11]]),
        };
        ensure!(p.w > 0 && p.h > 0, "pointer view size must be positive");
        Ok(p)
    }
}

/// Clear routing header the gateway puts in front of every record it
/// forwards over the host tunnel. Only `route_id` is acted on; the two times
/// are the gateway's residence for display and are not integrity protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub route_id: u64,
    pub gw_rx_us: u32,
    pub gw_tx_us: u32,
}

impl Route {
    pub const LEN: usize = 16;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..8].copy_from_slice(&self.route_id.to_be_bytes());
        out[8..12].copy_from_slice(&self.gw_rx_us.to_be_bytes());
        out[12..].copy_from_slice(&self.gw_tx_us.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8])> {
        ensure!(bytes.len() >= Self::LEN, "short route header");
        Ok((
            Self {
                route_id: u64::from_be_bytes(bytes[..8].try_into()?),
                gw_rx_us: u32::from_be_bytes(bytes[8..12].try_into()?),
                gw_tx_us: u32::from_be_bytes(bytes[12..16].try_into()?),
            },
            &bytes[Self::LEN..],
        ))
    }
}

/// WebSocket close codes, the same meanings veynode/core's account mesh uses.
pub mod close {
    pub const UNAUTHORIZED: u16 = 4001;
    pub const REPLACED: u16 = 4002;
    pub const REVOKED: u16 = 4003;
    pub const INVALID: u16 = 4009;
    pub const RATE_LIMITED: u16 = 4008;
    pub const RETRY: u16 = 4503;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip_and_partial() {
        let bytes = encode(CONTROL, br#"{"keyframe_req":{}}"#).unwrap();
        assert_eq!(&bytes[..5], &[0, 0, 0, 20, CONTROL]);
        assert_eq!(decode(&bytes[..10]).unwrap(), None);
        let (kind, payload, used) = decode(&bytes).unwrap().unwrap();
        assert_eq!((kind, used), (CONTROL, bytes.len()));
        assert_eq!(payload, br#"{"keyframe_req":{}}"#);
    }

    #[test]
    fn record_rejects_unknown_zero_and_oversized() {
        assert!(encode(0, b"").is_err());
        assert!(decode(&[0, 0, 0, 0, 1]).is_err());
        assert!(decode(&[0, 0, 0, 1, 42]).is_err());
        let mut big = (MAX_JSON as u32 + 2).to_be_bytes().to_vec();
        big.push(INPUT);
        big.resize(4 + MAX_JSON + 2, b' ');
        assert!(decode(&big).is_err());
    }

    #[test]
    fn video_headers_round_trip() {
        let h = VideoHeader {
            epoch: 3,
            frame_seq: 9,
            keyframe: true,
            sps_changed: true,
            pts_us: 1_000_000,
            encode_us: 4100,
            host_queue_us: 250,
            input_ack_seq: 77,
            view: View {
                x: 100,
                y: 50,
                w: 640,
                h: 400,
                src_w: 1280,
                src_h: 800,
            },
        };
        assert!(!h.view.is_full());
        assert!(View { x: 1000, ..h.view }.validate().is_err());
        let payload = h.encode(&[0, 0, 0, 1, 0x65]);
        let (back, au) = VideoHeader::decode(&payload).unwrap();
        assert_eq!((back, au), (h, &[0, 0, 0, 1, 0x65][..]));
        let mut bad = payload.clone();
        bad[8] = 4;
        assert!(VideoHeader::decode(&bad).is_err());
        let v1 = encode_video_v1(5, false, &[1, 2]);
        assert_eq!(decode_video_v1(&v1).unwrap(), (5, false, &[1, 2][..]));
    }

    #[test]
    fn pointer_and_route_round_trip() {
        let p = Pointer {
            seq: 1,
            x: -2,
            y: 700,
            w: 1280,
            h: 800,
        };
        assert_eq!(Pointer::decode(&p.encode()).unwrap(), p);
        assert!(Pointer::decode(&[0; 11]).is_err());
        let r = Route {
            route_id: 7,
            gw_rx_us: 1,
            gw_tx_us: 2,
        };
        let mut bytes = r.encode().to_vec();
        bytes.push(9);
        assert_eq!(Route::decode(&bytes).unwrap(), (r, &[9][..]));
    }
}
