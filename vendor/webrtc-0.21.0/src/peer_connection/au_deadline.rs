//! Bounded local AU submission deadlines. Neither capture time nor delivery proof.
use super::au_provenance::{Budget, Proof};
use rtc::rtp_transceiver::rtp_sender::RTCRtpSendParameters;
use std::sync::Arc;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub(crate) const CAPACITY: usize = 128;
pub(crate) const MAX_BUDGET: Duration = Duration::from_millis(250);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub ssrc: u32,
    pub payload: u8,
    pub timestamp: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub primary: (u32, u8),
    pub rtx: Option<(u32, u8)>,
}
impl Binding {
    pub fn from_parameters(
        p: &RTCRtpSendParameters,
        ssrc: u32,
        pt: u8,
    ) -> Result<Self, &'static str> {
        if p.encodings.len() != 1 || ssrc == 0 || pt > 127 || (64..=95).contains(&pt) {
            return Err("au_binding");
        }
        let coding = &p.encodings[0].rtp_coding_parameters;
        if coding.ssrc != Some(ssrc) || coding.fec.is_some() || !coding.rid.is_empty() {
            return Err("au_binding");
        }
        let codecs = &p.rtp_parameters.codecs;
        if codecs.iter().any(|c| {
            matches!(
                c.rtp_codec.mime_type.to_ascii_lowercase().as_str(),
                "video/red" | "video/ulpfec" | "video/flexfec" | "video/flexfec-03"
            )
        }) {
            return Err("au_binding");
        }
        if codecs
            .iter()
            .filter(|c| {
                c.payload_type == pt
                    && matches!(
                        c.rtp_codec.mime_type.to_ascii_lowercase().as_str(),
                        "video/h264" | "video/h265"
                    )
            })
            .count()
            != 1
        {
            return Err("au_binding");
        }
        let mut rtx_pt = None;
        for codec in codecs
            .iter()
            .filter(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case("video/rtx"))
        {
            let apt: Vec<_> = codec
                .rtp_codec
                .sdp_fmtp_line
                .split(';')
                .filter_map(|s| s.trim().strip_prefix("apt="))
                .collect();
            if apt.len() != 1 {
                return Err("au_binding");
            }
            let associated = apt[0].parse::<u8>().map_err(|_| "au_binding")?;
            if associated == pt && rtx_pt.replace(codec.payload_type).is_some() {
                return Err("au_binding");
            }
        }
        // Core enables RTX only when BOTH an RTX SSRC and a matching apt exist.
        let rtx = coding.rtx.as_ref().zip(rtx_pt).map(|(r, p)| (r.ssrc, p));
        if rtx.is_some_and(|(s, p)| {
            s == 0 || s == ssrc || p == pt || p > 127 || (64..=95).contains(&p)
        }) {
            return Err("au_binding");
        }
        Ok(Self {
            primary: (ssrc, pt),
            rtx,
        })
    }
    pub fn accepts(self, id: Identity) -> bool {
        self.primary == (id.ssrc, id.payload) || self.rtx == Some((id.ssrc, id.payload))
    }
}
struct Entry {
    timestamp: u32,
    deadline: Instant,
    proof: Arc<Proof>,
}
pub(crate) struct Table {
    pub binding: Binding,
    pub budget: Arc<Budget>,
    entries: VecDeque<Entry>,
    last: Option<u32>,
    span: u64,
    pub registered: u64,
    pub high_water: usize,
}
impl Table {
    pub fn new(binding: Binding) -> Self {
        Self {
            binding,
            budget: Arc::new(Budget::default()),
            entries: VecDeque::with_capacity(CAPACITY),
            last: None,
            span: 0,
            registered: 0,
            high_water: 0,
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn register(
        &mut self,
        timestamp: u32,
        deadline: Instant,
        now: Instant,
    ) -> Result<(), &'static str> {
        if deadline <= now {
            return Err("au_deadline_expired");
        }
        if deadline.duration_since(now) > MAX_BUDGET {
            return Err("au_registration_deadline");
        }
        let delta = self
            .last
            .map_or(0, |last| timestamp.wrapping_sub(last) as u64);
        if self.last.is_some() && (delta == 0 || delta >= 1 << 31 || self.span + delta >= 1 << 31) {
            return Err("au_timestamp_alias");
        }
        // Removal does not allow timestamp reuse: last/span survive the entry.
        if self.entries.len() == CAPACITY {
            // Preserve expired identities until capacity is needed, so routine
            // late NACKs can report known expiry instead of unknown provenance.
            if let Some(index) = self.entries.iter().position(|e| e.deadline <= now) {
                self.entries.remove(index);
            }
        }
        if self.entries.len() == CAPACITY {
            return Err("au_capacity");
        }
        let proof = Proof::new(self.budget.clone(), self.binding, timestamp, deadline)?;
        self.last = Some(timestamp);
        self.span += delta;
        self.entries.push_back(Entry {
            timestamp,
            deadline,
            proof,
        });
        self.registered = self.registered.saturating_add(1);
        self.high_water = self.high_water.max(self.entries.len());
        Ok(())
    }
    pub fn proof(&self, id: Identity) -> Result<Arc<Proof>, &'static str> {
        if !self.binding.accepts(id) {
            return Err("au_binding");
        }
        self.entries
            .iter()
            .find(|e| e.timestamp == id.timestamp)
            .map(|e| e.proof.clone())
            .ok_or("au_unknown")
    }
    pub fn deadline(&self, id: Identity) -> Result<Instant, &'static str> {
        if !self.binding.accepts(id) {
            return Err("au_binding");
        }
        self.entries
            .iter()
            .find(|e| e.timestamp == id.timestamp)
            .map(|e| e.deadline)
            .ok_or("au_unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn table() -> Table {
        Table::new(Binding {
            primary: (7, 108),
            rtx: Some((8, 109)),
        })
    }
    #[test]
    fn wrap_retirement_and_identity_are_not_new_eligibility() {
        let now = Instant::now();
        let mut t = table();
        let d = now + Duration::from_millis(100);
        t.register(u32::MAX - 20, d, now).unwrap();
        t.register(10, d, now).unwrap();
        assert_eq!(
            t.deadline(Identity {
                ssrc: 8,
                payload: 109,
                timestamp: 10
            })
            .unwrap(),
            d
        );
        assert!(
            t.deadline(Identity {
                ssrc: 9,
                payload: 109,
                timestamp: 10
            })
            .is_err()
        );
        assert_eq!(t.register(10, d, now), Err("au_timestamp_alias"));
        assert_eq!(t.register(u32::MAX - 20, d, now), Err("au_timestamp_alias"));
        let later = now + Duration::from_millis(101);
        t.register(20, later + Duration::from_millis(100), later)
            .unwrap();
        assert_eq!(
            t.deadline(Identity {
                ssrc: 7,
                payload: 108,
                timestamp: 10
            }),
            Ok(d)
        );
        assert!(
            t.register(10, later + Duration::from_millis(100), later)
                .is_err()
        );
    }
    #[test]
    fn capacity_and_incarnations_are_independent() {
        let now = Instant::now();
        let d = now + MAX_BUDGET;
        let mut a = table();
        let mut b = table();
        for ts in 0..CAPACITY as u32 {
            a.register(ts, d, now).unwrap();
        }
        assert_eq!(a.register(128, d, now), Err("au_capacity"));
        assert_eq!(a.len(), CAPACITY);
        b.register(0, now + Duration::from_millis(1), now).unwrap();
        assert_ne!(
            a.deadline(Identity {
                ssrc: 7,
                payload: 108,
                timestamp: 0
            })
            .unwrap(),
            b.deadline(Identity {
                ssrc: 7,
                payload: 108,
                timestamp: 0
            })
            .unwrap()
        );
        assert_eq!(a.register(1 << 31, d, now), Err("au_timestamp_alias"));
    }
}

#[cfg(test)]
pub(crate) fn test_parameters() -> RTCRtpSendParameters {
    use rtc::rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    };
    let mut p = RTCRtpSendParameters::default();
    p.encodings.push(RTCRtpEncodingParameters {
        rtp_coding_parameters: RTCRtpCodingParameters {
            ssrc: Some(7),
            ..Default::default()
        },
        ..Default::default()
    });
    p.rtp_parameters.codecs.push(RTCRtpCodecParameters {
        payload_type: 108,
        rtp_codec: RTCRtpCodec {
            mime_type: "video/H264".into(),
            clock_rate: 90000,
            ..Default::default()
        },
    });
    p
}
#[cfg(test)]
mod binding_tests {
    use super::*;
    use rtc::rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpRtxParameters,
    };
    #[test]
    fn actual_coding_and_apt_must_both_bind_rtx() {
        let mut p = test_parameters();
        p.encodings[0].rtp_coding_parameters.rtx = Some(RTCRtpRtxParameters { ssrc: 8 });
        assert_eq!(Binding::from_parameters(&p, 7, 108).unwrap().rtx, None);
        p.rtp_parameters.codecs.push(RTCRtpCodecParameters {
            payload_type: 109,
            rtp_codec: RTCRtpCodec {
                mime_type: "video/rtx".into(),
                clock_rate: 90000,
                sdp_fmtp_line: "apt=108;rtx-time=3000".into(),
                ..Default::default()
            },
        });
        assert_eq!(
            Binding::from_parameters(&p, 7, 108).unwrap().rtx,
            Some((8, 109))
        );
        p.encodings[0].rtp_coding_parameters.rtx = None;
        assert_eq!(Binding::from_parameters(&p, 7, 108).unwrap().rtx, None);
        p.rtp_parameters.codecs[1].rtp_codec.sdp_fmtp_line = "apt=108;apt=108".into();
        assert!(Binding::from_parameters(&p, 7, 108).is_err());
    }
    #[test]
    fn foreign_encoding_fec_and_unsupported_primary_are_rejected() {
        let p = test_parameters();
        assert!(Binding::from_parameters(&p, 8, 108).is_err());
        let mut p2 = p.clone();
        p2.encodings.push(p2.encodings[0].clone());
        assert!(Binding::from_parameters(&p2, 7, 108).is_err());
        for mime in ["video/red", "video/flexfec-03", "audio/opus"] {
            let mut p = p.clone();
            p.rtp_parameters.codecs[0].rtp_codec.mime_type = mime.into();
            assert!(Binding::from_parameters(&p, 7, 108).is_err());
        }
    }
    #[test]
    fn pressure_retires_only_expired_without_reusing_identity() {
        let now = Instant::now();
        let mut t = Table::new(Binding {
            primary: (7, 108),
            rtx: None,
        });
        for i in 0..128 {
            t.register(
                i,
                now + Duration::from_millis(if i == 50 { 10 } else { 200 }),
                now,
            )
            .unwrap();
        }
        let later = now + Duration::from_millis(11);
        t.register(128, later + Duration::from_millis(100), later)
            .unwrap();
        assert_eq!(t.len(), 128);
        assert_eq!(
            t.deadline(Identity {
                ssrc: 7,
                payload: 108,
                timestamp: 50
            }),
            Err("au_unknown")
        );
        assert!(
            t.deadline(Identity {
                ssrc: 7,
                payload: 108,
                timestamp: 0
            })
            .is_ok()
        );
        assert_eq!(
            t.register(50, later + Duration::from_millis(100), later),
            Err("au_timestamp_alias")
        );
    }
}
