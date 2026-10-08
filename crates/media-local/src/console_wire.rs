use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{Read, Write};

pub const MAX_BODY: usize = 4 * 1024 * 1024;
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
    if size == 0 || size > MAX_BODY || !matches!(header[4], 1 | 2) {
        bail!("invalid console frame length/type")
    }
    let epoch = u64::from_be_bytes(header[5..].try_into().unwrap());
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    if header[4] == 2 && (epoch == 0 || size <= 9 || body[8] > 1) {
        bail!("invalid console AU")
    }
    Ok(Record {
        kind: header[4],
        epoch,
        body,
    })
}
pub fn write_record(writer: &mut impl Write, record: &Record) -> Result<()> {
    if record.body.is_empty() || record.body.len() > MAX_BODY {
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
        for (len, kind) in [(0u32, 1), ((MAX_BODY + 1) as u32, 1), (1, 3)] {
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
}
