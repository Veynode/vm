//! The daemon's line protocol: response bounds, frames and socket buffers.
use super::*;

pub const MAX_MESSAGE: usize = 1024 * 1024;
/// A lossless 2560x1440 game frame is several MiB, so the PNG bound is 8 MiB (it was 2 MiB, which refused every real
/// scene at that size). The reply is base64 JSON: 8 MiB of PNG is about 10.7 MiB on the wire, hence the 12 MiB response
/// bound. The native console's `VEYNODE_SCREENSHOT_MAX_BYTES` and record writer carry the same pair.
pub const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SCREENSHOT_RESPONSE: usize = 12 * 1024 * 1024;
pub fn response_limit(method: &str) -> usize {
    // A 1 MiB driver frame inside the worker's and the guest agent's envelopes.
    if method == "screenshot" || method == "driver_rpc" {
        MAX_SCREENSHOT_RESPONSE
    } else {
        MAX_MESSAGE
    }
}
pub use veynodevm_mcp::response_limit as mcp_response_limit;
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    read_frame_bounded(reader, MAX_MESSAGE)
}
pub fn read_frame_bounded(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>> {
    if limit != MAX_MESSAGE && limit != MAX_SCREENSHOT_RESPONSE {
        bail!("invalid response bound")
    }
    let mut out = Vec::new();
    let n = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut out)?;
    if n == 0 {
        return Ok(None);
    }
    if n > limit || out.last() != Some(&b'\n') {
        bail!("oversized or unterminated JSON frame")
    }
    out.pop();
    Ok(Some(out))
}
/// One frame, handed to the descriptor in few writes: the serializer emits every token separately, and an
/// unbuffered socket or pipe would take a system call (and wake its reader) for each. A string larger than the
/// 8 KiB buffer still goes out directly, so no second copy of a screenshot is made.
pub(crate) fn write_json(writer: &mut impl Write, value: &Value) -> Result<()> {
    let mut buffered = std::io::BufWriter::new(&mut *writer);
    let result = (|| {
        serde_json::to_writer(&mut buffered, value)?;
        buffered.write_all(b"\n")?;
        buffered.flush()?;
        Ok(())
    })();
    // Discard, never flush again from the destructor: a failed frame gets no second write attempt.
    let _ = buffered.into_parts();
    result
}
/// Kernel send buffer requested for a connection about to carry a screenshot reply. macOS moves a Unix stream
/// in steps of the sender's buffer, 8 KiB by default: several hundred reader wake-ups for one 4 MiB reply.
pub const LARGE_REPLY_SEND_BUFFER: usize = 1024 * 1024;
/// Best effort; a refusal only leaves the default buffer. Bounds, deadlines and framing are unchanged: the kernel
/// may hold up to this many unread bytes for the peer instead of 8 KiB, for as long as the write deadline allows.
pub fn widen_send_buffer(socket: &UnixStream) {
    let size = LARGE_REPLY_SEND_BUFFER as libc::c_int;
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        );
    }
}
/// Reader buffer for a stream that may carry a screenshot reply: one read per pipe or socket buffer, not per 8 KiB.
pub(crate) const LARGE_REPLY_READ_BUFFER: usize = 64 * 1024;
