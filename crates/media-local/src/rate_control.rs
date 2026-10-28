//! Private local actuator, independent of media payloads or congestion policy.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::net::UnixStream,
    },
    time::{Duration, Instant},
};
pub const MIN_BPS: u64 = 250_000;
pub const MAX_BPS: u64 = 40_000_000;
// Half-framing-limit AU ceiling: (4 * 1024 * 1024) / 2 = 2,097,152 bytes.
// crates/media-local/testdata/encoder-rate-live/{h264,hevc}-result.json: max(records[].bytes) = 987,750 and 1,687,652;
// records[0].bytes = 821,922 and 1,437,745 (initial_status.target_bps = 8e6).
// 2 MiB is the next power of two above the measured maximum, with
// 2,097,152 - 1,687,652 = 409,500 bytes of headroom, inside the 4 MiB
// frame_schedule::frame_header limit. These are decoded hardware AUs, though
// the recordings failed their separate bitrate-response check.
// MAX_BPS caps the encoder target, not link throughput: even 40e6 * .250 / 8
// = 1,250,000 bytes would reject valid HEVC startup AUs. This size ceiling
// does not promise delivery; the existing 250 ms egress deadline still applies.
pub const MAX_ENCODED_ACCESS_UNIT_BYTES: usize = (4 * 1024 * 1024) / 2;
pub const MAX_CONSECUTIVE_OVERSIZED_UNITS: u64 = 3;
const SAFE: u64 = 9_007_199_254_740_991;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u64,
    pub request_id: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_bps: Option<u64>,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && !self.request_id.is_empty()
                && self.request_id.len() <= 64
                && self
                    .request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "invalid_request"
        );
        match self.method.as_str() {
            "rate_status" => ensure!(
                self.encoder_instance.is_none()
                    && self.epoch.is_none()
                    && self.sequence.is_none()
                    && self.target_bps.is_none(),
                "invalid_request"
            ),
            "set_bitrate" => ensure!(
                self.encoder_instance.as_deref().is_some_and(hex)
                    && self.epoch.is_some_and(|v| (1..=SAFE).contains(&v))
                    && self.sequence.is_some_and(|v| (1..=SAFE).contains(&v))
                    && self
                        .target_bps
                        .is_some_and(|v| (MIN_BPS..=MAX_BPS).contains(&v)),
                "invalid_request"
            ),
            _ => bail!("invalid_request"),
        }
        Ok(())
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 1024 && bytes.last() == Some(&b'\n'),
            "invalid rate frame"
        );
        let r: Self = serde_json::from_slice(bytes)?;
        let raw: serde_json::Value = serde_json::from_slice(bytes)?;
        ensure!(
            !raw.as_object()
                .context("invalid rate object")?
                .values()
                .any(serde_json::Value::is_null),
            "invalid_request"
        );
        r.validate()?;
        Ok(r)
    }
}
fn hex(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub encoder_instance: String,
    pub epoch: u64,
    pub sequence: u64,
    pub target_bps: u64,
    pub readback_bps: u64,
    pub submitted_frames: u64,
    pub hardware_verified: bool,
}
impl Receipt {
    pub fn validate(&self, request: &Request, epoch: u64) -> Result<()> {
        ensure!(
            hex(&self.encoder_instance)
                && self.epoch == epoch
                && epoch > 0
                && epoch <= SAFE
                && self.sequence <= SAFE
                && self.submitted_frames <= SAFE
                && self.hardware_verified
                && (MIN_BPS..=MAX_BPS).contains(&self.target_bps)
                && self.readback_bps == self.target_bps,
            "invalid rate acknowledgement"
        );
        if request.method == "set_bitrate" {
            ensure!(
                request.encoder_instance.as_deref() == Some(&self.encoder_instance)
                    && request.epoch == Some(self.epoch)
                    && request.sequence == Some(self.sequence)
                    && request.target_bps == Some(self.target_bps),
                "stale rate acknowledgement"
            );
        }
        Ok(())
    }
}
pub fn error_code(s: &str) -> &'static str {
    match s {
        "encoder_not_ready" => "encoder_not_ready",
        "stale_encoder" => "stale_encoder",
        "stale_sequence" => "stale_sequence",
        "invalid_request" => "invalid_request",
        "rate_control_fenced" => "rate_control_fenced",
        "rate_property_failed" => "rate_property_failed",
        "rate_readback_failed" => "rate_readback_failed",
        _ => "encoder_operation_failed",
    }
}
/// Owned inherited descriptor; no filesystem listener or ambient discovery.
pub struct Channel {
    socket: UnixStream,
    buffer: Vec<u8>,
    partial: Option<Instant>,
    pub pending: Option<(Request, Instant)>,
}
impl Channel {
    /// # Safety
    /// `fd` must be a uniquely owned inherited descriptor; ownership transfers
    /// here once its socket type and address family are validated.
    pub unsafe fn from_fd(fd: RawFd) -> Result<Self> {
        ensure!(fd > 2, "invalid private rate descriptor");
        let mut ty = 0;
        let mut n = std::mem::size_of::<i32>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    (&mut ty as *mut i32).cast(),
                    &mut n,
                )
            } == 0
                && ty == libc::SOCK_STREAM,
            "rate descriptor must be stream"
        );
        let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockname(
                    fd,
                    (&mut address as *mut libc::sockaddr_storage).cast(),
                    &mut length,
                )
            } == 0
                && address.ss_family as i32 == libc::AF_UNIX,
            "rate descriptor must be AF_UNIX"
        );
        let socket = unsafe { UnixStream::from_raw_fd(fd) };
        socket.local_addr()?;
        socket.peer_addr()?;
        socket.set_nonblocking(true)?;
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "rate descriptor flags"
        );
        Ok(Self {
            socket,
            buffer: Vec::new(),
            partial: None,
            pending: None,
        })
    }
    pub fn poll(&mut self) -> Result<Option<Request>> {
        if let Some((_, sent)) = &self.pending {
            ensure!(
                sent.elapsed() < Duration::from_secs(3),
                "rate acknowledgement deadline"
            );
            return Ok(None);
        }
        if let Some(start) = self.partial {
            ensure!(
                start.elapsed() < Duration::from_millis(500),
                "partial rate request deadline"
            )
        }
        loop {
            let mut byte = [0];
            match self.socket.read(&mut byte) {
                Ok(0) => bail!("private rate controller closed"),
                Ok(_) => {
                    self.partial.get_or_insert_with(Instant::now);
                    self.buffer.push(byte[0]);
                    ensure!(self.buffer.len() <= 1024, "rate request limit");
                    if byte[0] == b'\n' {
                        let r = Request::parse(&self.buffer)?;
                        let mut queued = 0u8;
                        ensure!(
                            unsafe {
                                libc::recv(
                                    self.socket.as_raw_fd(),
                                    (&mut queued as *mut u8).cast(),
                                    1,
                                    libc::MSG_PEEK,
                                )
                            } <= 0,
                            "concurrent rate request rejected"
                        );
                        self.buffer.clear();
                        self.partial = None;
                        self.pending = Some((r.clone(), Instant::now()));
                        return Ok(Some(r));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    pub fn reply(&mut self, value: serde_json::Value) -> Result<()> {
        let (request, _) = self.pending.as_ref().context("no pending rate request")?;
        let mut result = serde_json::json!({"version":1,"request_id":request.request_id});
        if let Some(v) = value.get("result") {
            result["result"] = v.clone()
        } else {
            result["error"] =
                serde_json::json!({"code":error_code(value["error"].as_str().unwrap_or(""))})
        }
        let mut bytes = serde_json::to_vec(&result)?;
        bytes.push(b'\n');
        ensure!(bytes.len() <= 1024, "rate response limit");
        let end = Instant::now() + Duration::from_millis(500);
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(Instant::now() < end, "rate response deadline");
            match self.socket.write(&bytes[offset..]) {
                Ok(0) => bail!("rate response closed"),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.pending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::fd::IntoRawFd;
    #[test]
    fn schema_rejects_duplicates_null_floats_bool_unknown_and_bounds() {
        for raw in [
            "{\"version\":1,\"version\":1,\"request_id\":\"x\",\"method\":\"rate_status\"}\n",
            "{\"version\":true,\"request_id\":\"x\",\"method\":\"rate_status\"}\n",
            "{\"version\":1,\"request_id\":\"x\",\"method\":\"rate_status\",\"epoch\":null}\n",
        ] {
            assert!(Request::parse(raw.as_bytes()).is_err())
        }
        let base = json!({"version":1,"request_id":"x","method":"set_bitrate","encoder_instance":"a".repeat(32),"epoch":1,"sequence":1,"target_bps":MIN_BPS});
        for (key, value) in [
            ("target_bps", json!(MIN_BPS - 1)),
            ("target_bps", json!(MAX_BPS + 1)),
            ("sequence", json!(1.0)),
            ("epoch", json!(false)),
            ("extra", json!(1)),
        ] {
            let mut v = base.clone();
            v[key] = value;
            assert!(Request::parse(format!("{v}\n").as_bytes()).is_err());
        }
        assert!(Request::parse(format!("{base}\n").as_bytes()).is_ok());
    }
    #[test]
    fn real_socket_partial_deadline_and_single_inflight() {
        let (mut parent, child) = UnixStream::pair().unwrap();
        let mut channel = unsafe { Channel::from_fd(child.into_raw_fd()) }.unwrap();
        parent.write_all(b"{").unwrap();
        assert!(channel.poll().unwrap().is_none());
        channel.partial = Some(Instant::now() - Duration::from_secs(1));
        assert!(channel.poll().is_err());
        let (mut parent, child) = UnixStream::pair().unwrap();
        let mut channel = unsafe { Channel::from_fd(child.into_raw_fd()) }.unwrap();
        parent
            .write_all(b"{\"version\":1,\"request_id\":\"q\",\"method\":\"rate_status\"}\n")
            .unwrap();
        assert!(channel.poll().unwrap().is_some());
        assert!(channel.poll().unwrap().is_none());
        channel.pending.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(4);
        assert!(channel.poll().is_err());
    }
    #[test]
    fn real_socket_blocked_reply_has_total_deadline_and_retains_pending() {
        let (_parent, child) = UnixStream::pair().unwrap();
        let mut channel = unsafe { Channel::from_fd(child.into_raw_fd()) }.unwrap();
        let amount: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    channel.socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&amount as *const libc::c_int).cast(),
                    std::mem::size_of_val(&amount) as libc::socklen_t,
                )
            },
            0
        );
        let mut filled = 0;
        loop {
            match channel.socket.write(&[0; 4096]) {
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected fill {other:?}"),
            };
            assert!(filled < 1024 * 1024);
        }
        let request =
            Request::parse(b"{\"version\":1,\"request_id\":\"q\",\"method\":\"rate_status\"}\n")
                .unwrap();
        channel.pending = Some((request, Instant::now()));
        let begin = Instant::now();
        assert!(channel.reply(json!({"error":"encoder_not_ready"})).is_err());
        assert!(
            begin.elapsed() >= Duration::from_millis(500)
                && begin.elapsed() < Duration::from_secs(2)
        );
        assert!(
            channel.pending.is_some(),
            "partial/failed ACK must not free the request slot"
        );
    }
}
