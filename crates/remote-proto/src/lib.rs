//! Remote desktop wire protocol shared by `vm-gateway` and the Mac host.
//!
//! Pure data and policy: framing, end-to-end records, datagram fragments,
//! the per-viewer pacer, adaptive bitrate, rate limits, TURN credentials and
//! the shared configuration file. No sockets, no async runtime. The browser
//! mirror lives in `web/src/remote/`; `tests/vectors.json` pins the bytes both
//! sides must produce. See `docs/remote-desktop.md`.
pub mod abr;
pub mod config;
pub mod frag;
pub mod hops;
pub mod keys;
pub mod pacer;
pub mod ratelimit;
pub mod record;
pub mod secure;
pub mod tunnel;
pub mod turn;

/// Protocol version carried in `hello`.
pub const PROTOCOL: u32 = 1;
