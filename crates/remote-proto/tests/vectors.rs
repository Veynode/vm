//! The byte vectors in web/src/remote/vectors.json, which the TypeScript
//! client also checks: both sides must produce and accept the same bytes.
use serde_json::Value;
use std::collections::BTreeMap;
use vm_remote_proto::frag;
use vm_remote_proto::record::{self, close, Pointer, VideoHeader, View};
use vm_remote_proto::secure::{Direction, Ephemeral, Opener, Sealer, SessionKeys};

fn vectors() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/src/remote/vectors.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hex(v: &Value) -> Vec<u8> {
    let s = v.as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn num(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap()
}

fn keys(v: &Value) -> SessionKeys {
    let e = &v["ecdh"];
    let transcript: [u8; 32] = hex(&e["transcript_sha256"]).try_into().unwrap();
    SessionKeys::from_shared(&hex(&e["shared"]), &transcript).unwrap()
}

#[test]
fn ecdh_and_key_schedule() {
    let v = vectors();
    let e = &v["ecdh"];
    let transcript: [u8; 32] = hex(&e["transcript_sha256"]).try_into().unwrap();
    let browser = Ephemeral::from_scalar(&hex(&e["browser_scalar"])).unwrap();
    let host = Ephemeral::from_scalar(&hex(&e["host_scalar"])).unwrap();
    assert_eq!(browser.public_key().unwrap(), hex(&e["browser_public"]));
    assert_eq!(host.public_key().unwrap(), hex(&e["host_public"]));
    let from_host = host
        .derive(&hex(&e["browser_public"]), &transcript)
        .unwrap();
    let from_browser = browser
        .derive(&hex(&e["host_public"]), &transcript)
        .unwrap();
    for (dir, name) in [
        (Direction::HostToBrowser, "k_h2b"),
        (Direction::BrowserToHost, "k_b2h"),
    ] {
        assert_eq!(from_host.key(dir).to_vec(), hex(&e[name]), "{name}");
        assert_eq!(from_browser.key(dir).to_vec(), hex(&e[name]), "{name}");
    }
}

#[test]
fn records() {
    for r in vectors()["records"].as_array().unwrap() {
        let kind = r["kind"].as_u64().unwrap() as u8;
        let payload = hex(&r["payload"]);
        let bytes = hex(&r["record"]);
        assert_eq!(
            record::encode(kind, &payload).unwrap(),
            bytes,
            "{}",
            r["name"]
        );
        let (k, p, used) = record::decode(&bytes).unwrap().unwrap();
        assert_eq!(
            (k, p, used),
            (kind, &payload[..], bytes.len()),
            "{}",
            r["name"]
        );
    }
}

#[test]
fn video_v2_header() {
    let v = &vectors()["video_v2"];
    let view = &v["view"];
    let field = |k: &str| num(&view[k]) as u16;
    let header = VideoHeader {
        epoch: num(&v["epoch"]) as u32,
        frame_seq: num(&v["frame_seq"]) as u32,
        keyframe: v["key"].as_bool().unwrap(),
        sps_changed: v["sps_changed"].as_bool().unwrap(),
        pts_us: num(&v["pts_us"]),
        encode_us: num(&v["encode_us"]) as u32,
        host_queue_us: num(&v["host_queue_us"]) as u32,
        input_ack_seq: num(&v["input_ack_seq"]) as u32,
        view: View {
            x: field("x"),
            y: field("y"),
            w: field("w"),
            h: field("h"),
            src_w: field("src_w"),
            src_h: field("src_h"),
        },
    };
    let annex_b = hex(&v["annex_b"]);
    let payload = hex(&v["payload"]);
    assert_eq!(header.encode(&annex_b), payload);
    let (decoded, rest) = VideoHeader::decode(&payload).unwrap();
    assert_eq!((decoded, rest), (header, &annex_b[..]));
}

#[test]
fn pointer() {
    let v = &vectors()["pointer"];
    let p = Pointer {
        seq: num(&v["seq"]) as u32,
        x: v["x"].as_i64().unwrap() as i16,
        y: v["y"].as_i64().unwrap() as i16,
        w: num(&v["w"]) as u16,
        h: num(&v["h"]) as u16,
    };
    assert_eq!(p.encode().to_vec(), hex(&v["payload"]));
    assert_eq!(Pointer::decode(&hex(&v["payload"])).unwrap(), p);
}

#[test]
fn sealed_records_both_ways() {
    let v = vectors();
    let keys = keys(&v);
    let mut by_dir: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for s in v["sealed"].as_array().unwrap() {
        by_dir
            .entry(s["direction"].as_str().unwrap())
            .or_default()
            .push(s);
    }
    for (dir_name, mut list) in by_dir {
        let dir = match dir_name {
            "h2b" => Direction::HostToBrowser,
            "b2h" => Direction::BrowserToHost,
            d => panic!("direction {d}"),
        };
        list.sort_by_key(|s| num(&s["seq"]));
        let mut sealer = Sealer::new(&keys, dir).unwrap();
        let mut opener = Opener::new(&keys, dir).unwrap();
        let mut next = 0;
        for s in list {
            let (kind, seq) = (num(&s["kind"]) as u8, num(&s["seq"]));
            let plaintext = hex(&s["plaintext"]);
            let sealed = hex(&s["sealed"]);
            assert_eq!(sealed[..9], hex(&s["aad"])[..], "{}", s["name"]);
            assert_eq!(
                seal_at(keys.key(dir), dir, seq, kind, &plaintext),
                sealed,
                "{}",
                s["name"]
            );
            // The sealer counts from 0; walk it there when the gap is small.
            if seq - next <= 1024 {
                while next < seq {
                    sealer.seal(kind, b"skip").unwrap();
                    next += 1;
                }
                assert_eq!(
                    sealer.seal(kind, &plaintext).unwrap(),
                    sealed,
                    "{}",
                    s["name"]
                );
                next += 1;
            }
            assert_eq!(
                opener.open(&sealed).unwrap(),
                (kind, seq, plaintext),
                "{}",
                s["name"]
            );
            assert!(opener.open(&sealed).is_err(), "replay of {}", s["name"]);
        }
    }
}

/// The record layout spelled out with the AEAD directly, independent of
/// [`Sealer`]'s counter.
fn seal_at(key: &[u8; 16], dir: Direction, seq: u64, kind: u8, plaintext: &[u8]) -> Vec<u8> {
    use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, key).unwrap());
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&(dir as u32).to_be_bytes());
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    let mut out = vec![kind];
    out.extend_from_slice(&seq.to_be_bytes());
    let mut body = plaintext.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(out.clone()),
        &mut body,
    )
    .unwrap();
    out.extend_from_slice(&body);
    out
}

#[test]
fn fragments() {
    let v = &vectors()["fragments"];
    let expected: Vec<Vec<u8>> = v["fragments"].as_array().unwrap().iter().map(hex).collect();
    let payload = hex(&v["payload"]);
    let got = frag::split(
        num(&v["epoch"]) as u32,
        num(&v["frame_seq"]) as u32,
        &payload,
    )
    .unwrap();
    assert_eq!(got, expected);
}

#[test]
fn close_codes() {
    let c = &vectors()["close_codes"];
    for (name, code) in [
        ("unauthorized", close::UNAUTHORIZED),
        ("replaced", close::REPLACED),
        ("revoked", close::REVOKED),
        ("rate_limited", close::RATE_LIMITED),
        ("invalid", close::INVALID),
        ("retry", close::RETRY),
    ] {
        assert_eq!(num(&c[name]), code as u64, "{name}");
    }
}
