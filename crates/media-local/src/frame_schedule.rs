//! Relative sender lag, not capture time or receiver latency.
//!
//! Native PTS is host-monotonic microseconds since the first encode submission.
//! Its absolute origin is not on the wire. The first complete AU establishes a
//! local reference; the lowest observed arrival-minus-PTS offset refines it.
//! A constant initial delay is unknowable, as is Guest capture-to-encode delay.
use anyhow::{bail, Context, Result};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_RELATIVE_LAG: Duration = Duration::from_millis(250);
pub const RECOVERY_LIMIT: Duration = Duration::from_secs(2);
pub const MAX_RECOVERY_DROPS: u64 = 120;
pub const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default, Debug)]
pub struct Stats {
    pub stale_dropped: u64,
    pub dependency_dropped: u64,
    pub recoveries: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Send { deadline: Instant },
    Drop { request_keyframe: bool },
}
struct Recovery {
    began: Instant,
    drops: u64,
    last_request: Option<Instant>,
}
#[derive(Default)]
pub struct Schedule {
    first_arrival: Option<Instant>,
    minimum_offset_us: i128,
    recovery: Option<Recovery>,
    stats: Stats,
}
impl Schedule {
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn recovery_deadline(&self) -> Option<Instant> {
        self.recovery.as_ref().map(|r| r.began + RECOVERY_LIMIT)
    }
    pub fn classify(&mut self, pts_us: u64, keyframe: bool, now: Instant) -> Result<Decision> {
        if let Some(recovery) = &self.recovery {
            if now >= recovery.began + RECOVERY_LIMIT || recovery.drops >= MAX_RECOVERY_DROPS {
                bail!("relative sender lag keyframe recovery exhausted")
            }
        }
        let first = *self.first_arrival.get_or_insert(now);
        let elapsed = now
            .checked_duration_since(first)
            .context("sender clock moved backwards")?;
        let offset = i128::try_from(elapsed.as_micros())? - i128::from(pts_us);
        self.minimum_offset_us = self.minimum_offset_us.min(offset);
        let lag_us = u64::try_from(offset - self.minimum_offset_us)?;
        let lag = Duration::from_micros(lag_us);
        let stale = lag >= MAX_RELATIVE_LAG;
        let dependency = self.recovery.is_some() && !keyframe;
        if stale || dependency {
            if stale {
                self.stats.stale_dropped += 1;
            } else {
                self.stats.dependency_dropped += 1;
            }
            let recovery = self.recovery.get_or_insert(Recovery {
                began: now,
                drops: 0,
                last_request: None,
            });
            recovery.drops += 1;
            let request = recovery
                .last_request
                .is_none_or(|at| now.duration_since(at) >= KEYFRAME_INTERVAL);
            if request {
                recovery.last_request = Some(now);
            }
            return Ok(Decision::Drop {
                request_keyframe: request,
            });
        }
        if self.recovery.take().is_some() {
            self.stats.recoveries += 1;
        }
        // Packetization time and each individual RTP queue wait consume the
        // remaining budget. A timeout after partial RTP submission is fatal;
        // already submitted packets cannot be retracted or called delivered.
        Ok(Decision::Send {
            deadline: now
                + MAX_RELATIVE_LAG
                    .saturating_sub(lag)
                    .min(Duration::from_millis(500)),
        })
    }
}

pub struct Frame {
    pub pts: u64,
    pub key: bool,
    pub data: Vec<u8>,
}
pub async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    previous: Option<u64>,
    recovery_deadline: Option<Instant>,
) -> Result<Frame> {
    let read = async {
        let mut header = [0_u8; 13];
        reader
            .read_exact(&mut header)
            .await
            .context("native frame header")?;
        let (length, pts, key) = frame_header(header, previous)?;
        let mut data = vec![0_u8; length];
        reader
            .read_exact(&mut data)
            .await
            .context("native frame body")?;
        Ok(Frame { pts, key, data })
    };
    if let Some(deadline) = recovery_deadline {
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), read)
            .await
            .context("relative sender lag keyframe recovery timed out")?
    } else {
        read.await
    }
}
pub fn frame_header(header: [u8; 13], previous: Option<u64>) -> Result<(usize, u64, bool)> {
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    let pts = u64::from_be_bytes(header[4..12].try_into().unwrap());
    if previous.is_none() && pts != 0 {
        bail!("native timeline must start at zero")
    }
    if length == 0
        || length > 4 * 1024 * 1024
        || header[12] > 1
        || previous.is_some_and(|last| pts <= last || pts - last > 10_000_000)
    {
        bail!("invalid or discontinuous native frame header")
    }
    Ok((length, pts, header[12] == 1))
}

pub async fn enqueue_before<T, E: Into<anyhow::Error>>(
    deadline: Instant,
    enqueue: impl Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    if Instant::now() >= deadline {
        bail!("relative sender lag expired before RTP submission")
    }
    let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), enqueue)
        .await
        .context("relative sender lag expired during RTP submission")?
        .map_err(Into::into)?;
    if Instant::now() >= deadline {
        bail!("relative sender lag expired after RTP submission")
    }
    Ok(result)
}

// Both PLI/FIR and scheduler recovery use one serialized writer. A queued
// request is not success: failed/timed-out native stdin writes propagate to
// the session's existing failure/cleanup path.
pub struct KeyframeWriter<'a, W> {
    writer: &'a mut W,
    last_request: Option<Instant>,
}
impl<'a, W: AsyncWrite + Unpin> KeyframeWriter<'a, W> {
    pub fn new(writer: &'a mut W) -> Self {
        Self {
            writer,
            last_request: None,
        }
    }
    pub async fn request(&mut self) -> Result<()> {
        if let Some(last) = self.last_request {
            tokio::time::sleep_until(tokio::time::Instant::from_std(last + KEYFRAME_INTERVAL))
                .await;
        }
        tokio::time::timeout(
            Duration::from_millis(100),
            self.writer.write_all(b"keyframe\n"),
        )
        .await
        .context("native keyframe write deadline")??;
        self.last_request = Some(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[test]
    fn idle_and_variable_cadence_are_not_frame_age() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        for (pts, elapsed) in [
            (0, 0),
            (1_000_000, 1_000),
            (1_033_333, 1_034),
            (3_000_000, 3_002),
        ] {
            assert!(matches!(
                schedule
                    .classify(pts, false, now + Duration::from_millis(elapsed))
                    .unwrap(),
                Decision::Send { .. }
            ));
        }
        assert_eq!(schedule.stats().stale_dropped, 0);
    }
    #[test]
    fn stale_keyframes_and_fresh_dependent_frames_drop_until_fresh_keyframe() {
        let now = Instant::now();
        let mut s = Schedule::default();
        assert!(matches!(
            s.classify(0, true, now).unwrap(),
            Decision::Send { .. }
        ));
        assert_eq!(
            s.classify(10_000, false, now + Duration::from_millis(300))
                .unwrap(),
            Decision::Drop {
                request_keyframe: true
            }
        );
        assert_eq!(
            s.classify(20_000, true, now + Duration::from_millis(310))
                .unwrap(),
            Decision::Drop {
                request_keyframe: false
            }
        );
        assert_eq!(
            s.classify(330_000, false, now + Duration::from_millis(340))
                .unwrap(),
            Decision::Drop {
                request_keyframe: false
            }
        );
        assert!(matches!(
            s.classify(350_000, true, now + Duration::from_millis(350))
                .unwrap(),
            Decision::Send { .. }
        ));
        assert_eq!(
            (
                s.stats().stale_dropped,
                s.stats().dependency_dropped,
                s.stats().recoveries
            ),
            (2, 1, 1)
        );
    }
    #[test]
    fn initial_unknown_delay_is_not_claimed_and_low_water_offset_refines_reference() {
        let arrival = Instant::now() + Duration::from_secs(5);
        let mut s = Schedule::default();
        assert!(
            matches!(s.classify(0, true, arrival).unwrap(), Decision::Send { .. }),
            "absolute delay before first arrival is unknown"
        );
        assert!(matches!(
            s.classify(1_000_000, false, arrival + Duration::from_millis(800))
                .unwrap(),
            Decision::Send { .. }
        ));
        assert_eq!(
            s.classify(1_100_000, false, arrival + Duration::from_millis(1_200))
                .unwrap(),
            Decision::Drop {
                request_keyframe: true
            },
            "300ms newer lag relative to improved offset"
        );
    }
    #[test]
    fn recovery_has_time_and_drop_count_bounds_without_rebasing_pts() {
        let now = Instant::now();
        let mut s = Schedule::default();
        s.classify(0, true, now).unwrap();
        s.classify(1, false, now + Duration::from_millis(300))
            .unwrap();
        assert!(s
            .classify(2_300_000, true, now + Duration::from_millis(2300))
            .is_err());
        let mut s = Schedule::default();
        s.classify(0, true, now).unwrap();
        for pts in 1..=MAX_RECOVERY_DROPS {
            s.classify(pts, false, now + Duration::from_millis(300))
                .unwrap();
        }
        assert!(s
            .classify(400_000, true, now + Duration::from_millis(400))
            .is_err());
    }
    #[test]
    fn keyframe_request_is_coalesced_and_deadline_is_remaining_lag_budget() {
        let now = Instant::now();
        let mut s = Schedule::default();
        s.classify(0, true, now).unwrap();
        assert_eq!(
            s.classify(1, false, now + Duration::from_millis(300))
                .unwrap(),
            Decision::Drop {
                request_keyframe: true
            }
        );
        assert_eq!(
            s.classify(2, false, now + Duration::from_millis(799))
                .unwrap(),
            Decision::Drop {
                request_keyframe: false
            }
        );
        assert_eq!(
            s.classify(3, false, now + Duration::from_millis(800))
                .unwrap(),
            Decision::Drop {
                request_keyframe: true
            }
        );
        let mut s = Schedule::default();
        s.classify(0, true, now).unwrap();
        assert_eq!(
            s.classify(100_000, false, now + Duration::from_millis(300))
                .unwrap(),
            Decision::Send {
                deadline: now + Duration::from_millis(350)
            }
        );
    }
    #[tokio::test]
    async fn real_unix_stream_discards_old_and_dependent_frames_without_pts_rewrite() {
        // Real OS socket framing with synthetic AU bytes; no codec/network acceptance.
        let (mut tx, mut rx) = tokio::net::UnixStream::pair().unwrap();
        let begin = Instant::now();
        let mut schedule = Schedule::default();
        let mut previous = None;
        let mut sent = Vec::new();
        let mut requests = 0;
        let producer = tokio::spawn(async move {
            for (pts, key, delay) in [
                (0, true, 0),
                (10_000, false, 300),
                (320_000, false, 20),
                (340_000, true, 20),
            ] {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let mut frame = Vec::new();
                frame.extend_from_slice(&1u32.to_be_bytes());
                frame.extend_from_slice(&(pts as u64).to_be_bytes());
                frame.push(u8::from(key));
                frame.push(7);
                tx.write_all(&frame).await.unwrap();
            }
        });
        for _ in 0..4 {
            let f = read_frame(&mut rx, previous, schedule.recovery_deadline())
                .await
                .unwrap();
            previous = Some(f.pts);
            match schedule.classify(f.pts, f.key, Instant::now()).unwrap() {
                Decision::Send { deadline } => {
                    enqueue_before(deadline, async {
                        sent.push(f.pts);
                        Ok::<(), anyhow::Error>(())
                    })
                    .await
                    .unwrap();
                }
                Decision::Drop { request_keyframe } => requests += usize::from(request_keyframe),
            }
        }
        producer.await.unwrap();
        assert_eq!(sent, [0, 340_000]);
        assert_eq!(requests, 1);
        assert!(begin.elapsed() < Duration::from_secs(2));
    }
    #[tokio::test]
    async fn partial_frame_recovery_read_cannot_wait_forever() {
        let (mut tx, mut rx) = tokio::net::UnixStream::pair().unwrap();
        tx.write_all(&[0, 0, 0, 1]).await.unwrap();
        assert!(read_frame(
            &mut rx,
            Some(0),
            Some(Instant::now() + Duration::from_millis(30))
        )
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("recovery timed out"));
    }
    #[tokio::test]
    async fn every_packet_uses_same_deadline_and_partial_submission_is_error() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let deadline = Instant::now() + Duration::from_millis(30);
        enqueue_before(deadline, tx.send(1)).await.unwrap();
        assert!(enqueue_before(deadline, tx.send(2)).await.is_err());
        assert_eq!(rx.recv().await, Some(1));
        assert!(rx.try_recv().is_err());
        assert!(enqueue_before(deadline, async {
            panic!("expired send must not be polled");
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        })
        .await
        .is_err());
    }
    #[tokio::test]
    async fn keyframe_requests_write_actual_bounded_commands_and_fail_on_closed_pipe() {
        let (mut writer, mut reader) = tokio::net::UnixStream::pair().unwrap();
        let mut requests = KeyframeWriter::new(&mut writer);
        requests.request().await.unwrap();
        let mut command = [0; 9];
        reader.read_exact(&mut command).await.unwrap();
        assert_eq!(&command, b"keyframe\n");
        drop(reader);
        assert!(
            requests.request().await.is_err(),
            "native write failure must not be treated as successful request"
        );
    }
    #[tokio::test]
    async fn blocked_keyframe_writer_has_a_real_deadline() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let began = Instant::now();
        assert!(KeyframeWriter::new(&mut writer)
            .request()
            .await
            .unwrap_err()
            .to_string()
            .contains("write deadline"));
        assert!(began.elapsed() < Duration::from_secs(1));
    }
}
