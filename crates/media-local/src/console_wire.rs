use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{Read, Write};

/// Bound for a video access unit (kind 2) and the default for any record.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
/// Bound for a JSON record (kind 1). A screenshot reply is base64 PNG: up to 8 MiB of PNG is about 10.7 MiB, so 12 MiB, the
/// same pair as `vm_control::MAX_SCREENSHOT_BYTES`/`MAX_SCREENSHOT_RESPONSE` and the native `RecordWriter`.
pub const MAX_JSON_BODY: usize = 12 * 1024 * 1024;
/// Bound for a Guest audio record (kind 3): `u32 BE rate, u8 channels, s16le PCM`.
pub const MAX_AUDIO_BODY: usize = 64 * 1024 + 5;
/// The body bound for a record kind; unknown kinds are not valid records.
pub fn body_limit(kind: u8) -> Option<usize> {
    match kind {
        1 => Some(MAX_JSON_BODY),
        2 => Some(MAX_BODY),
        3 => Some(MAX_AUDIO_BODY),
        _ => None,
    }
}
/// The sample rate and channel count of a well-formed audio record body.
pub fn audio_format(body: &[u8]) -> Option<(u32, u8)> {
    let rate = u32::from_be_bytes(body.get(..4)?.try_into().ok()?);
    let channels = *body.get(4)?;
    let pcm = body.len() - 5;
    ((8_000..=96_000).contains(&rate)
        && (1..=2).contains(&channels)
        && pcm > 0
        && pcm.is_multiple_of(2 * channels as usize))
    .then_some((rate, channels))
}
#[derive(Clone, Debug)]
pub struct Record {
    pub kind: u8,
    pub epoch: u64,
    pub body: Vec<u8>,
}
impl Record {
    pub fn json(epoch: u64, value: &Value) -> Self {
        Self {
            kind: 1,
            epoch,
            body: serde_json::to_vec(value).expect("JSON Value is serializable"),
        }
    }
    pub fn value(&self) -> Result<Value> {
        if self.kind != 1 {
            bail!("record is not JSON")
        }
        serde_json::from_slice(&self.body).context("invalid console JSON")
    }
}
pub fn read_record(reader: &mut impl Read) -> Result<Record> {
    let mut header = [0; 13];
    reader
        .read_exact(&mut header)
        .context("console frame header")?;
    let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    if size == 0 || body_limit(header[4]).is_none_or(|limit| size > limit) {
        bail!("invalid console frame length/type")
    }
    let epoch = u64::from_be_bytes(header[5..].try_into().unwrap());
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    if header[4] == 2 && (epoch == 0 || size <= 9 || body[8] > 1) {
        bail!("invalid console AU")
    }
    if header[4] == 3 && (epoch != 0 || audio_format(&body).is_none()) {
        bail!("invalid console audio")
    }
    Ok(Record {
        kind: header[4],
        epoch,
        body,
    })
}
pub fn write_record(writer: &mut impl Write, record: &Record) -> Result<()> {
    if record.body.is_empty()
        || body_limit(record.kind).is_none_or(|limit| record.body.len() > limit)
    {
        bail!("invalid record size")
    }
    let mut header = [0; 13];
    header[..4].copy_from_slice(&(record.body.len() as u32).to_be_bytes());
    header[4] = record.kind;
    header[5..].copy_from_slice(&record.epoch.to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(&record.body)?;
    writer.flush()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_rejects_unbounded_wrong_kind_and_partial_records() {
        for (len, kind) in [
            (0u32, 1),
            ((MAX_JSON_BODY + 1) as u32, 1),
            ((MAX_BODY + 1) as u32, 2),
            (1, 3),
            (1, 4),
        ] {
            let mut h = [0; 13];
            h[..4].copy_from_slice(&len.to_be_bytes());
            h[4] = kind;
            assert!(read_record(&mut &h[..]).is_err());
        }
        let original = Record::json(7, &serde_json::json!({"id":"x","result":true}));
        let mut bytes = Vec::new();
        write_record(&mut bytes, &original).unwrap();
        for size in 0..bytes.len() {
            assert!(read_record(&mut &bytes[..size]).is_err());
        }
        let parsed = read_record(&mut bytes.as_slice()).unwrap();
        assert_eq!(parsed.epoch, 7);
        assert_eq!(parsed.value().unwrap(), original.value().unwrap());
    }
    #[test]
    fn json_records_may_carry_a_screenshot_but_access_units_stay_at_four_mib() {
        let big = "a".repeat(5 * 1024 * 1024);
        let record = Record::json(
            3,
            &serde_json::json!({"id":"shot","result":{"data_base64":big}}),
        );
        assert!(record.body.len() > MAX_BODY && record.body.len() < MAX_JSON_BODY);
        let mut bytes = Vec::new();
        write_record(&mut bytes, &record).unwrap();
        let parsed = read_record(&mut bytes.as_slice()).unwrap();
        assert_eq!(parsed.body.len(), record.body.len());
        let access_unit = Record {
            kind: 2,
            epoch: 1,
            body: vec![0; MAX_BODY + 1],
        };
        assert!(write_record(&mut Vec::new(), &access_unit).is_err());
        assert_eq!(body_limit(1), Some(MAX_JSON_BODY));
        assert_eq!(body_limit(2), Some(MAX_BODY));
        assert_eq!(body_limit(4), None);
    }
    #[test]
    fn audio_records_carry_a_bounded_well_formed_format() {
        let mut body = 48_000u32.to_be_bytes().to_vec();
        body.push(2);
        body.extend_from_slice(&[0; 3840]);
        assert_eq!(audio_format(&body), Some((48_000, 2)));
        let record = Record {
            kind: 3,
            epoch: 0,
            body: body.clone(),
        };
        let mut bytes = Vec::new();
        write_record(&mut bytes, &record).unwrap();
        assert_eq!(read_record(&mut bytes.as_slice()).unwrap().body, body);
        // Epoch-scoped, odd-sized, three-channel or out-of-range audio is refused.
        bytes[12] = 1;
        assert!(read_record(&mut bytes.as_slice()).is_err());
        for bad in [
            [&48_000u32.to_be_bytes()[..], &[2, 0, 0, 0]].concat(),
            [&48_000u32.to_be_bytes()[..], &[3, 0, 0, 0, 0, 0, 0]].concat(),
            [&4_000u32.to_be_bytes()[..], &[1, 0, 0]].concat(),
            [&48_000u32.to_be_bytes()[..], &[1]].concat(),
        ] {
            assert_eq!(audio_format(&bad), None);
        }
    }
}
