//! The byte-exact examples every implementation of API 1 tests against
//! (`tests/vectors/wire.json`; core's plugin reads the same file).
use serde_json::Value;
use veynodevm_api::{frame, open, Channel, ErrorCode};

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors/wire.json")).unwrap()
}

#[test]
fn frames_match_the_vectors() {
    for case in vectors()["frames"].as_array().unwrap() {
        let channel = Channel::from_u8(case["channel"].as_u64().unwrap() as u8).unwrap();
        let payload = hex(case["payload"].as_str().unwrap());
        let wire = hex(case["wire"].as_str().unwrap());
        assert_eq!(
            frame::encode(channel, &payload).unwrap(),
            wire,
            "{}",
            case["name"]
        );
        let (read_channel, read_payload) = frame::read_frame(&mut &wire[..]).unwrap().unwrap();
        assert_eq!((read_channel, read_payload), (channel as u8, payload));
    }
}

#[test]
fn lines_open_as_the_vectors_say() {
    let lines = vectors()["lines"].as_array().unwrap().clone();
    let by_name =
        |name: &str| lines.iter().find(|case| case["name"] == name).unwrap()["value"].clone();
    assert_eq!(open(by_name("stream start")).unwrap()["stream"], true);
    assert_eq!(
        open(by_name("refusal")).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        open(by_name("version refusal")).unwrap_err().code,
        ErrorCode::Version
    );
    let mut line = Vec::new();
    frame::write_line(&mut line, &by_name("hello request")).unwrap();
    assert_eq!(
        line,
        b"{\"arguments\":{\"api\":1,\"client\":\"veynode-core/1\"},\"method\":\"hello\"}\n"
    );
}
