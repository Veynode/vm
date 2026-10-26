//! Valid uncompressed PNG fixture, generated in memory without image dependencies.
//! This is protocol test data, not a Guest screenshot or render acceptance.
/// The same uncompressed fixture at another size; `png_sized(2560, 720)` is about 5.5 MiB.
pub fn png_sized(w: u32, h: u32) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut v = !0u32;
        for b in bytes {
            v ^= *b as u32;
            for _ in 0..8 {
                v = (v >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(v & 1)));
            }
        }
        !v
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let c = crc(&out[start..]);
        out.extend_from_slice(&c.to_be_bytes());
    }
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&[(x % 251) as u8, (y % 251) as u8, ((x + y) % 251) as u8]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let count = raw.chunks(65535).len();
    for (i, part) in raw.chunks(65535).enumerate() {
        z.push(u8::from(i + 1 == count));
        let n = part.len() as u16;
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(part);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
pub fn value() -> serde_json::Value {
    value_sized(1280, 320)
}
pub fn value_sized(w: u32, h: u32) -> serde_json::Value {
    use base64::Engine;
    let png = png_sized(w, h);
    serde_json::json!({"mime_type":"image/png","byte_length":png.len(),"data_base64":base64::engine::general_purpose::STANDARD.encode(png),"width":w,"height":h,"frame_id":"12345678-1234-4234-8234-123456789abc","captured_monotonic_us":1,"source_generation":1,"source_sequence":1})
}
