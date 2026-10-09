//! Strict outgoing shapes emitted by the pinned rtc-mdns implementation.
//! This is used only with the driver's dedicated multicast socket, never by
//! the generic peer-datagram classifier. No hostname/address is logged.
use rtc::{
    mdns::{MDNS_DEST_ADDR, MDNS_PORT},
    shared::{TransportContext, TransportProtocol},
};
use std::net::{IpAddr, SocketAddr};

pub(crate) fn transport_matches(t: &TransportContext) -> bool {
    t.transport_protocol == TransportProtocol::UDP
        && t.peer_addr == MDNS_DEST_ADDR
        && t.local_addr.is_ipv4()
        && t.local_addr.port() == MDNS_PORT
        && t.ecn.is_none()
}

pub(crate) fn validate(bytes: &[u8], t: &TransportContext, socket: SocketAddr) -> bool {
    if !transport_matches(t)
        || socket != SocketAddr::from(([0, 0, 0, 0], MDNS_PORT))
        || bytes.len() < 17
        || bytes.len() > 512
    {
        return false;
    }
    // The pinned producer emits a single uncompressed A/IN question or answer.
    // Its parser is private; exact counts/flags/length below reject any appended
    // record, compression pointer, non-A record or payload-shaped lookalike.
    let response = match &bytes[..12] {
        [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0] => false,
        [0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0] => true,
        _ => return false,
    };
    let mut p = 12;
    let mut labels = 0;
    let mut last = &[][..];
    loop {
        let Some(&n) = bytes.get(p) else {
            return false;
        };
        p += 1;
        if n == 0 {
            break;
        }
        if n > 63 {
            return false;
        }
        let Some(label) = bytes.get(p..p + usize::from(n)) else {
            return false;
        };
        last = label;
        labels += 1;
        p += usize::from(n);
        if p - 12 > 254 {
            return false;
        }
    }
    if labels < 2
        || !last.eq_ignore_ascii_case(b"local")
        || p - 12 > 255
        || bytes.get(p..p + 4) != Some(&[0, 1, 0, 1])
    {
        return false;
    }
    p += 4;
    if !response {
        return t.local_addr.ip().is_unspecified() && p == bytes.len();
    }
    // Existing producer: TTL120, exactly four A-address octets, matching its
    // source interface. IPv6 mDNS output is not implemented by pinned rtc-mdns.
    let IpAddr::V4(ip) = t.local_addr.ip() else {
        return false;
    };
    !ip.is_unspecified()
        && !ip.is_multicast()
        && bytes.get(p..p + 6) == Some(&[0, 0, 0, 120, 0, 4])
        && bytes.get(p + 6..) == Some(ip.octets().as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::{
        mdns::{Mdns, MdnsConfig},
        sansio::Protocol,
    };
    use std::time::{Duration, Instant};
    pub(crate) fn real_query() -> rtc::shared::TaggedBytesMut {
        let now = Instant::now();
        let mut mdns = Mdns::new(now, MdnsConfig::default());
        mdns.query_now(now, "synthetic-egress.local");
        mdns.poll_write().unwrap()
    }
    #[test]
    fn actual_library_query_retry_and_answer_shapes() {
        let now = Instant::now();
        let socket = "0.0.0.0:5353".parse().unwrap();
        let mut client = Mdns::new(now, MdnsConfig::default());
        client.query_now(now, "synthetic-egress.local");
        let query = client.poll_write().unwrap();
        assert!(validate(&query.message, &query.transport, socket));
        // This exact 1s retry is rejected by the generic SRTP/STUN classifier.
        assert!(super::super::media_egress::classify(&query.message, false).is_err());
        client.handle_timeout(now + Duration::from_secs(1)).unwrap();
        let retry = client.poll_write().unwrap();
        assert_eq!(query.message, retry.message);
        assert!(validate(&retry.message, &retry.transport, socket));
        let mut server = Mdns::new(
            now,
            MdnsConfig::default()
                .with_local_names(vec!["synthetic-egress.local".into()])
                .with_local_ip("192.0.2.1".parse().unwrap()),
        );
        server.handle_read(query).unwrap();
        let answer = server.poll_write().unwrap();
        assert!(validate(&answer.message, &answer.transport, socket));
    }
    #[test]
    fn wrong_socket_destination_trailing_record_flags_and_compression_refused() {
        let q = real_query();
        let socket = "0.0.0.0:5353".parse().unwrap();
        assert!(!validate(
            &q.message,
            &q.transport,
            "127.0.0.1:5353".parse().unwrap()
        ));
        let mut t = q.transport;
        t.peer_addr = "127.0.0.1:5353".parse().unwrap();
        assert!(!validate(&q.message, &t, socket));
        t = q.transport;
        t.peer_addr = "[ff02::fb]:5353".parse().unwrap();
        assert!(!validate(&q.message, &t, socket));
        t = q.transport;
        t.local_addr = "0.0.0.0:5000".parse().unwrap();
        assert!(!validate(&q.message, &t, socket));
        for i in 0..12 {
            let mut b = q.message.to_vec();
            b[i] ^= 1;
            assert!(!validate(&b, &q.transport, socket));
        }
        let mut b = q.message.to_vec();
        b.extend_from_slice(&[0, 0, 0, 0]);
        assert!(!validate(&b, &q.transport, socket));
        let mut b = q.message.to_vec();
        b[12] = 0xc0;
        assert!(!validate(&b, &q.transport, socket));
        for n in 0..q.message.len() {
            assert!(!validate(&q.message[..n], &q.transport, socket));
        }
    }
}
