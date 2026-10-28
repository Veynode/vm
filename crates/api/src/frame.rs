//! Lines before a stream, frames during one.
//!
//! A frame is `u8 channel | u32 big-endian length | payload`, the payload at
//! most [`FRAME_MAX`](crate::FRAME_MAX) bytes.
use crate::FRAME_MAX;
use anyhow::{bail, Result};
use serde_json::Value;
use std::io::{BufRead, Read, Write};

/// A stream's channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    /// Client to daemon; an empty frame ends the input.
    Stdin = 0,
    Stdout = 1,
    Stderr = 2,
    /// JSON, such as `{"progress":"…"}`.
    Event = 3,
    /// The call's answer envelope; the daemon closes after it.
    End = 4,
    /// Client to daemon, JSON: a signal, a resize or a method's own message.
    Control = 5,
}

impl Channel {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Stdin,
            1 => Self::Stdout,
            2 => Self::Stderr,
            3 => Self::Event,
            4 => Self::End,
            5 => Self::Control,
            _ => return None,
        })
    }
}

/// `payload` as one frame; a longer payload is an error, not split.
pub fn encode(channel: Channel, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > FRAME_MAX {
        bail!("frame payload over {FRAME_MAX} bytes")
    }
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(channel as u8);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Writes `payload` as one frame in a single write.
pub fn write_frame(writer: &mut impl Write, channel: Channel, payload: &[u8]) -> Result<()> {
    writer.write_all(&encode(channel, payload)?)?;
    Ok(())
}

/// Writes `data` as frames of at most `FRAME_MAX` bytes; empty data writes none.
pub fn write_chunked(writer: &mut impl Write, channel: Channel, data: &[u8]) -> Result<()> {
    for chunk in data.chunks(FRAME_MAX) {
        write_frame(writer, channel, chunk)?;
    }
    Ok(())
}

/// The next frame, or `None` at a clean end between frames. A channel this
/// side does not know is returned as its raw byte for the caller to skip.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    let mut got = 0;
    while got < head.len() {
        let n = reader.read(&mut head[got..])?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            bail!("stream ended inside a frame header")
        }
        got += n;
    }
    let length = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if length > FRAME_MAX {
        bail!("frame of {length} bytes is over {FRAME_MAX}")
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

/// One JSON line, at most `limit` bytes, or `None` at a clean end.
pub fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let n = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut out)?;
    if n == 0 {
        return Ok(None);
    }
    if n > limit || out.last() != Some(&b'\n') {
        bail!("oversized or unterminated JSON line")
    }
    out.pop();
    Ok(Some(out))
}

/// `value` as one line, handed over in one write.
pub fn write_line(writer: &mut impl Write, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut wire = Vec::new();
        write_frame(&mut wire, Channel::Stdout, b"hi").unwrap();
        write_frame(&mut wire, Channel::Stdin, b"").unwrap();
        assert_eq!(wire, [1, 0, 0, 0, 2, b'h', b'i', 0, 0, 0, 0, 0]);
        let mut reader = &wire[..];
        assert_eq!(read_frame(&mut reader).unwrap(), Some((1, b"hi".to_vec())));
        assert_eq!(read_frame(&mut reader).unwrap(), Some((0, vec![])));
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn oversized_and_torn_frames_are_refused() {
        assert!(encode(Channel::Stdout, &vec![0; FRAME_MAX + 1]).is_err());
        let head = [1u8, 0x00, 0x10, 0x00, 0x01];
        assert!(read_frame(&mut &head[..]).is_err());
        assert!(read_frame(&mut &[1u8, 0, 0][..]).is_err());
    }

    #[test]
    fn chunks_split_at_the_limit() {
        let mut wire = Vec::new();
        write_chunked(&mut wire, Channel::Stdin, &vec![7; FRAME_MAX + 3]).unwrap();
        let mut reader = &wire[..];
        assert_eq!(read_frame(&mut reader).unwrap().unwrap().1.len(), FRAME_MAX);
        assert_eq!(read_frame(&mut reader).unwrap().unwrap().1.len(), 3);
    }

    #[test]
    fn lines_are_bounded() {
        let mut reader = &b"{\"a\":1}\nrest"[..];
        assert_eq!(read_line(&mut reader, 16).unwrap().unwrap(), b"{\"a\":1}");
        assert!(read_line(&mut reader, 16).is_err());
        assert!(read_line(&mut &b"0123456789\n"[..], 4).is_err());
    }
}
