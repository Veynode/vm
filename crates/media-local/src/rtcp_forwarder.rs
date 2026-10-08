//! Bounded keyframe-feedback forwarding for one local diagnostic sender/peer.
//!
//! API pattern adapted from webrtc 0.21.0's official `rtcp-processing` example:
//! https://github.com/webrtc-rs/webrtc/blob/v0.21.0/examples/rtcp-processing/rtcp-processing.rs
//! Upstream is dual MIT/Apache-2.0; the adapted pattern is used under MIT below.
//! This notice does not select a license for the rest of Veynode.
//!
//! Copyright (c) 2021 WebRTC.rs
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//! The above copyright notice and this permission notice shall be included in
//! all copies or substantial portions of the Software.
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
//! THE SOFTWARE.
//!
//! Install at Slot::from(14_000), after default interceptors. It marks only
//! PLI/FIR addressed to the expected, currently bound local media SSRC. Receiver
//! reports, NACK and other feedback are left to the default chain. Other
//! attributes survive as empty carriers; incoming RTP is not supported by this
//! send-only diagnostic adapter. Make a fresh instance per peer/session.

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::{
    full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication,
};
use rtc::sansio::Protocol;
use rtc::shared::{error::Error, marshal::MarshalSize};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const READ_CAPACITY: usize = 8;
const WRITE_CAPACITY: usize = 32;
const MAX_PACKET_BYTES: usize = 64 * 1024;
const MAX_COMPOUND_PACKETS: usize = 64;
const REQUEST_INTERVAL: Duration = Duration::from_millis(500);
// Also bounds delivery to the framework's downstream application event channel
// if the application stops polling. The diagnostic server's lifetime is 180s.
const MAX_SESSION_REQUESTS: usize = 512;

pub struct RtcpForwarder {
    expected_ssrc: u32,
    bound: bool,
    last_emit: Option<Instant>,
    last_fir: Option<(u32, u8)>,
    emitted: usize,
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}
impl RtcpForwarder {
    pub fn new(expected_ssrc: u32) -> Self {
        Self {
            expected_ssrc,
            bound: false,
            last_emit: None,
            last_fir: None,
            emitted: 0,
            read_queue: VecDeque::new(),
            write_queue: VecDeque::new(),
        }
    }
    fn filtered_request(
        &mut self,
        packet: &dyn rtc::rtcp::Packet,
    ) -> Option<Box<dyn rtc::rtcp::Packet>> {
        if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
            return (pli.media_ssrc == self.expected_ssrc)
                .then(|| Box::new(pli.clone()) as Box<dyn rtc::rtcp::Packet>);
        }
        if let Some(fir) = packet.as_any().downcast_ref::<FullIntraRequest>() {
            // RFC 5104 FIR targets live in FCI, not the common-header media SSRC.
            let entry = fir
                .fir
                .iter()
                .take(MAX_COMPOUND_PACKETS)
                .find(|entry| entry.ssrc == self.expected_ssrc)?;
            let identity = (fir.sender_ssrc, entry.sequence_number);
            if self.last_fir == Some(identity) {
                return None;
            }
            self.last_fir = Some(identity);
            return Some(Box::new(FullIntraRequest {
                sender_ssrc: fir.sender_ssrc,
                media_ssrc: fir.media_ssrc,
                fir: vec![entry.clone()],
            }));
        }
        None
    }
}

/// Defense-in-depth for `TrackLocalEvent::OnRtcpPacket` before requesting native
/// keyframes. The peer and local track must still belong to the active session.
pub fn requests_keyframe(packets: &[Box<dyn rtc::rtcp::Packet>], expected_ssrc: u32) -> bool {
    packets.iter().take(MAX_COMPOUND_PACKETS).any(|packet| {
        if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
            return pli.media_ssrc == expected_ssrc;
        }
        packet
            .as_any()
            .downcast_ref::<FullIntraRequest>()
            .is_some_and(|fir| {
                fir.fir
                    .iter()
                    .take(MAX_COMPOUND_PACKETS)
                    .any(|entry| entry.ssrc == expected_ssrc)
            })
    })
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for RtcpForwarder {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        let Packet::Rtcp(packets) = &msg.message.packet else {
            return Ok(());
        };
        let eligible = self.bound
            && self.emitted < MAX_SESSION_REQUESTS
            && self.last_emit.is_none_or(|last| {
                msg.now
                    .checked_duration_since(last)
                    .is_some_and(|elapsed| elapsed >= REQUEST_INTERVAL)
            });
        let request = if eligible {
            packets
                .iter()
                .take(MAX_COMPOUND_PACKETS)
                .find_map(|packet| self.filtered_request(packet.as_ref()))
        } else {
            None
        };
        // Never trust an earlier broad delivery annotation to bypass this filter.
        msg.message
            .attributes
            .retain(|attribute| !matches!(attribute, Attribute::DeliverToApplication));
        msg.message.packet = Packet::Rtcp(request.into_iter().collect());
        if matches!(&msg.message.packet, Packet::Rtcp(packets) if !packets.is_empty()) {
            self.emitted += 1;
            self.last_emit = Some(msg.now);
            msg.message.add(Attribute::DeliverToApplication);
        } else if msg.message.attributes.is_empty() {
            return Ok(());
        }
        if self.read_queue.len() == READ_CAPACITY {
            self.read_queue.pop_front();
        }
        self.read_queue.push_back(msg);
        Ok(())
    }
    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if self.write_queue.len() >= WRITE_CAPACITY {
            return Err(Error::ErrBufferFull);
        }
        let bytes = match &msg.message.packet {
            Packet::Rtp(packet) => packet.marshal_size(),
            Packet::Rtcp(packets) if packets.len() <= MAX_COMPOUND_PACKETS => {
                packets.iter().fold(0usize, |total, packet| {
                    total.saturating_add(packet.marshal_size())
                })
            }
            _ => return Err(Error::ErrPacketTooBig),
        };
        if bytes > MAX_PACKET_BYTES {
            return Err(Error::ErrPacketTooBig);
        }
        self.write_queue.push_back(msg);
        Ok(())
    }
    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}
impl Interceptor for RtcpForwarder {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        if info.ssrc == self.expected_ssrc && info.ssrc != 0 {
            self.bound = true;
        }
    }
    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        if info.ssrc == self.expected_ssrc {
            self.bound = false;
            self.read_queue.clear();
            self.write_queue.clear();
        }
    }
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::interceptor::{AttributedPacket, Registry, Slot};
    use rtc::rtcp::{
        payload_feedbacks::full_intra_request::FirEntry, receiver_report::ReceiverReport,
    };
    const SSRC: u32 = 12_345_678;
    fn info() -> StreamInfo {
        StreamInfo {
            ssrc: SSRC,
            ..Default::default()
        }
    }
    fn pli(ssrc: u32) -> Box<dyn rtc::rtcp::Packet> {
        Box::new(PictureLossIndication {
            sender_ssrc: 99,
            media_ssrc: ssrc,
        })
    }
    fn message(now: Instant, packets: Vec<Box<dyn rtc::rtcp::Packet>>) -> TaggedPacket {
        TaggedPacket {
            now,
            transport: Default::default(),
            message: AttributedPacket::new(Packet::Rtcp(packets)),
        }
    }
    fn bound() -> RtcpForwarder {
        let mut forwarder = RtcpForwarder::new(SSRC);
        forwarder.bind_local_stream(&info());
        forwarder
    }

    #[test]
    fn bound_ssrc_only_and_annotation_survives_real_chain_terminus() {
        let now = Instant::now();
        let mut chain = Registry::new()
            .with(Slot::from(14_000), RtcpForwarder::new(SSRC))
            .build();
        chain.handle_read(message(now, vec![pli(SSRC)])).unwrap();
        assert!(chain.poll_read().is_none());
        chain.bind_local_stream(&info());
        chain
            .handle_read(message(now, vec![pli(SSRC + 1)]))
            .unwrap();
        assert!(chain.poll_read().is_none());
        chain
            .handle_read(message(
                now,
                vec![Box::new(ReceiverReport::default()), pli(SSRC)],
            ))
            .unwrap();
        let delivered = chain.poll_read().unwrap();
        assert!(delivered.message.has(&Attribute::DeliverToApplication));
        let Packet::Rtcp(packets) = delivered.message.packet else {
            panic!("not RTCP")
        };
        assert_eq!(packets.len(), 1);
        assert!(requests_keyframe(&packets, SSRC));
        chain.unbind_local_stream(&info());
        chain
            .handle_read(message(now + Duration::from_secs(1), vec![pli(SSRC)]))
            .unwrap();
        assert!(chain.poll_read().is_none());
    }
    #[test]
    fn fir_fci_filters_other_ssrc_and_suppresses_duplicate_sequence() {
        let now = Instant::now();
        let mut f = bound();
        let fir = FullIntraRequest {
            sender_ssrc: 99,
            media_ssrc: 0,
            fir: vec![
                FirEntry {
                    ssrc: 7,
                    sequence_number: 1,
                },
                FirEntry {
                    ssrc: SSRC,
                    sequence_number: 2,
                },
            ],
        };
        f.handle_read(message(now, vec![Box::new(fir.clone())]))
            .unwrap();
        let Packet::Rtcp(packets) = f.poll_read().unwrap().message.packet else {
            panic!()
        };
        let filtered = packets[0]
            .as_any()
            .downcast_ref::<FullIntraRequest>()
            .unwrap();
        assert_eq!(filtered.fir.len(), 1);
        assert_eq!(filtered.fir[0].ssrc, SSRC);
        f.handle_read(message(now + Duration::from_secs(1), vec![Box::new(fir)]))
            .unwrap();
        assert!(f.poll_read().is_none());
    }
    #[test]
    fn bounded_flood_rate_queue_and_session_budget() {
        let now = Instant::now();
        let mut f = bound();
        for _ in 0..10_000 {
            f.handle_read(message(now, vec![pli(SSRC)])).unwrap();
        }
        assert_eq!(f.emitted, 1);
        assert_eq!(f.read_queue.len(), 1);
        for i in 1..2000 {
            f.handle_read(message(now + REQUEST_INTERVAL * i, vec![pli(SSRC)]))
                .unwrap();
        }
        assert_eq!(f.emitted, MAX_SESSION_REQUESTS);
        assert_eq!(f.read_queue.len(), READ_CAPACITY);
    }
    #[test]
    fn unrelated_feedback_never_marked_but_control_attributes_survive() {
        let now = Instant::now();
        let mut f = bound();
        let mut msg = message(now, vec![Box::new(ReceiverReport::default())]);
        msg.message.add(Attribute::DeliverToApplication);
        msg.message.add(Attribute::TargetBitrateChanged {
            bits_per_second: 800_000.0,
        });
        f.handle_read(msg).unwrap();
        let out = f.poll_read().unwrap();
        assert!(!out.message.has(&Attribute::DeliverToApplication));
        assert!(out.message.has(&Attribute::TargetBitrateChanged {
            bits_per_second: 0.0
        }));
        assert!(matches!(out.message.packet, Packet::Rtcp(packets) if packets.is_empty()));
    }
    #[test]
    fn write_capacity_and_packet_size_fail_explicitly() {
        let now = Instant::now();
        let mut f = bound();
        for _ in 0..WRITE_CAPACITY {
            f.handle_write(message(now, vec![pli(SSRC)])).unwrap();
        }
        assert_eq!(
            f.handle_write(message(now, vec![pli(SSRC)])).unwrap_err(),
            Error::ErrBufferFull
        );
        for _ in 0..WRITE_CAPACITY {
            assert!(f.poll_write().is_some());
        }
        let mut msg = message(now, vec![]);
        msg.message.packet = Packet::Rtp(rtc::rtp::Packet {
            payload: bytes::Bytes::from(vec![0; MAX_PACKET_BYTES + 1]),
            ..Default::default()
        });
        assert_eq!(f.handle_write(msg).unwrap_err(), Error::ErrPacketTooBig);
    }
    #[test]
    fn teardown_drops_queued_requests_and_media() {
        let now = Instant::now();
        let mut f = bound();
        f.handle_read(message(now, vec![pli(SSRC)])).unwrap();
        f.handle_write(message(now, vec![pli(SSRC)])).unwrap();
        f.unbind_local_stream(&info());
        assert!(f.poll_read().is_none());
        assert!(f.poll_write().is_none());
    }
}
