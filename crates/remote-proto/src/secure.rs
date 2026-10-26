//! End-to-end records between the browser and the Mac host. The gateway
//! (and Cloudflare, in that deployment) sees only these.
//!
//! Keys: P-256 ECDH between per-session ephemeral keys, both signed in the
//! session-auth handshake, then HKDF-SHA256 with the handshake transcript
//! hash as salt into one AES-128-GCM key per direction. Record on the wire:
//! `u8 kind | u64 seq | ciphertext‖tag`, nonce `u32 direction ‖ u64 seq`,
//! AAD `u8 kind ‖ u64 seq`. The plaintext is the bare record payload; the
//! kind is in the AAD and the length comes from the framing. Receivers keep a
//! 4096-entry sliding window, so video may arrive out of order (WebTransport
//! streams and datagrams) but never twice, and a reliable record held behind
//! a datagram-mode keyframe (hundreds of fragments, one seq each) still fits.
//! Browser side: `web/src/remote/secure.ts`.
use anyhow::{anyhow, ensure, Result};
use aws_lc_rs::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM},
    agreement::{self, PrivateKey, UnparsedPublicKey, ECDH_P256},
    hkdf::{self, Salt, HKDF_SHA256},
};

pub const KEY_LEN: usize = 16;
pub const TAG_LEN: usize = 16;
pub const HEADER_LEN: usize = 9;
pub const PUBLIC_KEY_LEN: usize = 65;
const WINDOW: u64 = 4096;
const WORDS: usize = (WINDOW / 64) as usize;

/// Who seals: the host's records to the browser, or the browser's to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    HostToBrowser = 0,
    BrowserToHost = 1,
}

impl Direction {
    fn info(self) -> &'static [u8] {
        match self {
            Self::HostToBrowser => b"Veynode/remote-key/v1\0h2b",
            Self::BrowserToHost => b"Veynode/remote-key/v1\0b2h",
        }
    }
}

/// A per-session ephemeral P-256 key.
pub struct Ephemeral(PrivateKey);

impl Ephemeral {
    pub fn generate() -> Result<Self> {
        Ok(Self(
            PrivateKey::generate(&ECDH_P256).map_err(|_| anyhow!("ECDH key generation failed"))?,
        ))
    }

    /// From a 32-byte scalar; test vectors only.
    pub fn from_scalar(bytes: &[u8]) -> Result<Self> {
        Ok(Self(
            PrivateKey::from_private_key(&ECDH_P256, bytes)
                .map_err(|e| anyhow!("ECDH key: {e}"))?,
        ))
    }

    /// Uncompressed SEC1 point, the form WebCrypto's `raw` export uses.
    pub fn public_key(&self) -> Result<Vec<u8>> {
        let public = self
            .0
            .compute_public_key()
            .map_err(|_| anyhow!("ECDH public key"))?;
        let bytes = public.as_ref().to_vec();
        ensure!(
            bytes.len() == PUBLIC_KEY_LEN && bytes[0] == 4,
            "unexpected ECDH public key form"
        );
        Ok(bytes)
    }

    /// Both direction keys for a transcript, consuming the ephemeral key.
    pub fn derive(self, peer_public: &[u8], transcript_sha256: &[u8; 32]) -> Result<SessionKeys> {
        ensure!(
            peer_public.len() == PUBLIC_KEY_LEN && peer_public[0] == 4,
            "peer ECDH key must be an uncompressed P-256 point"
        );
        let peer = UnparsedPublicKey::new(&ECDH_P256, peer_public);
        let shared = agreement::agree(&self.0, peer, anyhow!("ECDH agreement failed"), |s| {
            Ok(s.to_vec())
        })?;
        SessionKeys::from_shared(&shared, transcript_sha256)
    }
}

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// The two AES-128-GCM keys of one session.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKeys {
    pub host_to_browser: [u8; KEY_LEN],
    pub browser_to_host: [u8; KEY_LEN],
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKeys(..)")
    }
}

impl SessionKeys {
    pub fn from_shared(shared: &[u8], transcript_sha256: &[u8; 32]) -> Result<Self> {
        let prk = Salt::new(HKDF_SHA256, transcript_sha256).extract(shared);
        let expand = |d: Direction| -> Result<[u8; KEY_LEN]> {
            let mut out = [0; KEY_LEN];
            prk.expand(&[d.info()], Len(KEY_LEN))
                .and_then(|okm| okm.fill(&mut out))
                .map_err(|_| anyhow!("HKDF expand failed"))?;
            Ok(out)
        };
        Ok(Self {
            host_to_browser: expand(Direction::HostToBrowser)?,
            browser_to_host: expand(Direction::BrowserToHost)?,
        })
    }

    pub fn key(&self, direction: Direction) -> &[u8; KEY_LEN] {
        match direction {
            Direction::HostToBrowser => &self.host_to_browser,
            Direction::BrowserToHost => &self.browser_to_host,
        }
    }
}

fn nonce(direction: Direction, seq: u64) -> Nonce {
    let mut n = [0; 12];
    n[..4].copy_from_slice(&(direction as u32).to_be_bytes());
    n[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

fn aad(kind: u8, seq: u64) -> [u8; HEADER_LEN] {
    let mut a = [0; HEADER_LEN];
    a[0] = kind;
    a[1..].copy_from_slice(&seq.to_be_bytes());
    a
}

fn aead_key(key: &[u8; KEY_LEN]) -> Result<LessSafeKey> {
    Ok(LessSafeKey::new(
        UnboundKey::new(&AES_128_GCM, key).map_err(|_| anyhow!("AES key"))?,
    ))
}

/// Seals one direction's records with strictly increasing sequence numbers.
pub struct Sealer {
    key: LessSafeKey,
    direction: Direction,
    next: u64,
}

impl Sealer {
    pub fn new(keys: &SessionKeys, direction: Direction) -> Result<Self> {
        Ok(Self {
            key: aead_key(keys.key(direction))?,
            direction,
            next: 0,
        })
    }

    pub fn seal(&mut self, kind: u8, plaintext: &[u8]) -> Result<Vec<u8>> {
        ensure!(crate::record::known(kind), "unknown record kind {kind}");
        let seq = self.next;
        self.next = seq
            .checked_add(1)
            .ok_or_else(|| anyhow!("record sequence exhausted"))?;
        let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
        out.extend(aad(kind, seq));
        let mut body = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                nonce(self.direction, seq),
                Aad::from(aad(kind, seq)),
                &mut body,
            )
            .map_err(|_| anyhow!("seal failed"))?;
        out.extend(body);
        Ok(out)
    }
}

/// Opens one direction's records, rejecting tampering and replays.
pub struct Opener {
    key: LessSafeKey,
    direction: Direction,
    /// Highest sequence accepted plus one; 0 before the first.
    top: u64,
    /// Bit `seq % WINDOW` set: `seq` was accepted (for seqs in the window).
    seen: [u64; WORDS],
}

impl Opener {
    pub fn new(keys: &SessionKeys, direction: Direction) -> Result<Self> {
        Ok(Self {
            key: aead_key(keys.key(direction))?,
            direction,
            top: 0,
            seen: [0; WORDS],
        })
    }

    /// `(kind, seq, plaintext)`. The window only moves after authentication.
    pub fn open(&mut self, record: &[u8]) -> Result<(u8, u64, Vec<u8>)> {
        ensure!(record.len() >= HEADER_LEN + TAG_LEN, "short sealed record");
        let kind = record[0];
        ensure!(crate::record::known(kind), "unknown record kind {kind}");
        let seq = u64::from_be_bytes(record[1..HEADER_LEN].try_into()?);
        ensure!(self.fresh(seq), "replayed or too old record {seq}");
        let mut body = record[HEADER_LEN..].to_vec();
        let plain = self
            .key
            .open_in_place(
                nonce(self.direction, seq),
                Aad::from(aad(kind, seq)),
                &mut body,
            )
            .map_err(|_| anyhow!("record authentication failed"))?
            .to_vec();
        self.accept(seq);
        Ok((kind, seq, plain))
    }

    fn fresh(&self, seq: u64) -> bool {
        if seq >= self.top {
            return true;
        }
        self.top - 1 - seq < WINDOW && !self.bit(seq)
    }

    fn bit(&self, seq: u64) -> bool {
        let i = seq % WINDOW;
        self.seen[(i / 64) as usize] & (1 << (i % 64)) != 0
    }

    fn set(&mut self, seq: u64, on: bool) {
        let i = seq % WINDOW;
        let word = &mut self.seen[(i / 64) as usize];
        if on {
            *word |= 1 << (i % 64);
        } else {
            *word &= !(1 << (i % 64));
        }
    }

    fn accept(&mut self, seq: u64) {
        if seq >= self.top {
            if seq - self.top >= WINDOW {
                self.seen = [0; WORDS];
            } else {
                for s in self.top..seq {
                    self.set(s, false);
                }
            }
            self.top = seq + 1;
        }
        self.set(seq, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record;

    fn pair() -> (SessionKeys, SessionKeys) {
        let transcript = [7; 32];
        let host = Ephemeral::generate().unwrap();
        let browser = Ephemeral::generate().unwrap();
        let (hp, bp) = (host.public_key().unwrap(), browser.public_key().unwrap());
        (
            host.derive(&bp, &transcript).unwrap(),
            browser.derive(&hp, &transcript).unwrap(),
        )
    }

    #[test]
    fn both_sides_derive_the_same_distinct_keys() {
        let (h, b) = pair();
        assert_eq!(h, b);
        assert_ne!(h.host_to_browser, h.browser_to_host);
    }

    #[test]
    fn seal_open_round_trip_and_direction_binding() {
        let (h, b) = pair();
        let mut seal = Sealer::new(&h, Direction::HostToBrowser).unwrap();
        let mut open = Opener::new(&b, Direction::HostToBrowser).unwrap();
        let r = seal.seal(record::EVENT, b"{\"ready\":{}}").unwrap();
        assert_eq!(
            open.open(&r).unwrap(),
            (record::EVENT, 0, b"{\"ready\":{}}".to_vec())
        );
        let mut wrong = Opener::new(&b, Direction::BrowserToHost).unwrap();
        let r = seal.seal(record::EVENT, b"x").unwrap();
        assert!(wrong.open(&r).is_err());
    }

    #[test]
    fn tampering_with_kind_seq_or_body_fails() {
        let (h, b) = pair();
        let mut seal = Sealer::new(&h, Direction::HostToBrowser).unwrap();
        let r = seal.seal(record::VIDEO_V2, &[1, 2, 3]).unwrap();
        for i in [0, 3, HEADER_LEN, r.len() - 1] {
            let mut t = r.clone();
            t[i] ^= if i == 0 { 3 } else { 1 };
            let mut open = Opener::new(&b, Direction::HostToBrowser).unwrap();
            assert!(open.open(&t).is_err(), "byte {i}");
        }
    }

    #[test]
    fn window_allows_reorder_rejects_replay_and_stale() {
        let (h, b) = pair();
        let mut seal = Sealer::new(&h, Direction::HostToBrowser).unwrap();
        let records: Vec<_> = (0..4300)
            .map(|_| seal.seal(record::VIDEO_V2, b"f").unwrap())
            .collect();
        let mut open = Opener::new(&b, Direction::HostToBrowser).unwrap();
        open.open(&records[5]).unwrap();
        open.open(&records[3]).unwrap();
        assert!(open.open(&records[5]).is_err());
        assert!(open.open(&records[3]).is_err());
        open.open(&records[4]).unwrap();
        open.open(&records[90]).unwrap();
        open.open(&records[20]).unwrap();
        assert!(open.open(&records[20]).is_err());
        open.open(&records[4200]).unwrap();
        assert!(open.open(&records[100]).is_err(), "older than the window");
        open.open(&records[200]).unwrap();
        assert!(open.open(&records[200]).is_err());
        assert!(open.open(&records[90]).is_err(), "slot reused, still stale");
    }

    #[test]
    fn failed_authentication_does_not_move_the_window() {
        let (h, b) = pair();
        let mut seal = Sealer::new(&h, Direction::HostToBrowser).unwrap();
        let r0 = seal.seal(record::PING, b"0").unwrap();
        let mut forged = seal.seal(record::PING, b"1").unwrap();
        forged[1..9].copy_from_slice(&1000u64.to_be_bytes());
        let mut open = Opener::new(&b, Direction::HostToBrowser).unwrap();
        assert!(open.open(&forged).is_err());
        open.open(&r0).unwrap();
    }
}
