// End-to-end secure records (plan §3.1 layer 3 and §3.3 step 5), matching
// `crates/remote-proto/src/secure.rs` byte for byte. The gateway sees only
// `u8 kind | u64 seq | ciphertext`; keys come from an ephemeral P-256 ECDH
// whose public halves the session-auth handshake signs.
//
//   shared   = ECDH(P-256) x-coordinate, 32 bytes
//   k_h2b    = HKDF-SHA256(shared, salt = transcript sha256, info = "Veynode/remote-key/v1\0h2b", 16)
//   k_b2h    = HKDF-SHA256(shared, salt = transcript sha256, info = "Veynode/remote-key/v1\0b2h", 16)
//   nonce    = direction (u32: h2b 0, b2h 1) ‖ seq (u64)
//   aad      = kind (u8) ‖ seq (u64)
//   record   = kind ‖ seq ‖ AES-128-GCM(k, nonce, aad, payload) ‖ tag(16)
//
// The plaintext is the record's payload (a fragment on the datagram video
// path); its kind is the authenticated header byte. One sequence space per
// direction, from 0, with a 4096-record replay window that moves only after a
// record authenticates.

export const DIRECTION = { h2b: 0, b2h: 1 } as const
export type Direction = keyof typeof DIRECTION

export const SECURE_HEADER = 9
export const TAG = 16
export const KEY_INFO = { h2b: 'Veynode/remote-key/v1\0h2b', b2h: 'Veynode/remote-key/v1\0b2h' } as const

const MAX_SEQ = (1n << 64n) - 1n

export class ReplayError extends Error {}
export class AuthError extends Error {}

export function nonce(direction: Direction, seq: bigint): Uint8Array {
  const out = new Uint8Array(12)
  const view = new DataView(out.buffer)
  view.setUint32(0, DIRECTION[direction])
  view.setBigUint64(4, seq)
  return out
}

export function aad(kind: number, seq: bigint): Uint8Array {
  const out = new Uint8Array(SECURE_HEADER)
  out[0] = kind
  new DataView(out.buffer).setBigUint64(1, seq)
  return out
}

/** Replay window size (secure.rs WINDOW): wide enough for a datagram key frame's fragments to pass a delayed reliable record. */
export const WINDOW = 4096
const WINDOW_N = BigInt(WINDOW)

/**
 * Receiver's replay window, as `crates/remote-proto/src/secure.rs` `Opener`:
 * a ring bitmap indexed by `seq % 4096`; `top` is one past the highest seq
 * accepted. Fresh: `seq >= top`, or within the window with its bit clear.
 */
export class ReplayWindow {
  private top = 0n
  private readonly bits = new Uint32Array(WINDOW / 32)

  private slot(seq: bigint) {
    const i = Number(seq % WINDOW_N)
    return { word: i >>> 5, mask: 1 << (i & 31) }
  }

  private bit(seq: bigint) {
    const { word, mask } = this.slot(seq)
    return (this.bits[word] & mask) !== 0
  }

  /** Whether `seq` is new and inside the window. Does not record it. */
  check(seq: bigint): boolean {
    if (seq >= this.top) return true
    return this.top - 1n - seq < WINDOW_N && !this.bit(seq)
  }

  /** Records `seq` once its record authenticated. */
  accept(seq: bigint) {
    if (seq >= this.top) {
      // Slots between the old top and `seq` belong to seqs never seen.
      if (seq - this.top >= WINDOW_N) this.bits.fill(0)
      else
        for (let s = this.top; s < seq; s++) {
          const { word, mask } = this.slot(s)
          this.bits[word] &= ~mask
        }
      this.top = seq + 1n
    }
    const { word, mask } = this.slot(seq)
    this.bits[word] |= mask
  }
}

const subtle = () => globalThis.crypto.subtle
const buf = (b: Uint8Array) => b as Uint8Array<ArrayBuffer>

export interface Ephemeral {
  privateKey: CryptoKey
  /** Uncompressed SEC1 point, 65 bytes, as signed into the offer/answer. */
  publicKey: Uint8Array
}

export async function generateEphemeral(): Promise<Ephemeral> {
  const pair = await subtle().generateKey({ name: 'ECDH', namedCurve: 'P-256' }, false, ['deriveBits'])
  return { privateKey: pair.privateKey, publicKey: new Uint8Array(await subtle().exportKey('raw', pair.publicKey)) }
}

export interface SessionKeys {
  h2b: CryptoKey
  b2h: CryptoKey
}

/** Both directions' AES-128-GCM keys from our private key, the peer's point and the transcript hash. */
export async function deriveKeys(privateKey: CryptoKey, peerPublic: Uint8Array, transcript: Uint8Array): Promise<SessionKeys> {
  const peer = await subtle().importKey('raw', buf(peerPublic), { name: 'ECDH', namedCurve: 'P-256' }, false, [])
  const shared = await subtle().deriveBits({ name: 'ECDH', public: peer }, privateKey, 256)
  const ikm = await subtle().importKey('raw', shared, 'HKDF', false, ['deriveKey'])
  const derive = (info: string) =>
    subtle().deriveKey(
      { name: 'HKDF', hash: 'SHA-256', salt: buf(transcript), info: new TextEncoder().encode(info) },
      ikm,
      { name: 'AES-GCM', length: 128 },
      false,
      ['encrypt', 'decrypt'],
    )
  return { h2b: await derive(KEY_INFO.h2b), b2h: await derive(KEY_INFO.b2h) }
}

export function importAesKey(raw: Uint8Array): Promise<CryptoKey> {
  return subtle().importKey('raw', buf(raw), 'AES-GCM', false, ['encrypt', 'decrypt'])
}

export async function seal(key: CryptoKey, direction: Direction, kind: number, seq: bigint, plain: Uint8Array): Promise<Uint8Array> {
  const ad = aad(kind, seq)
  const sealed = await subtle().encrypt({ name: 'AES-GCM', iv: buf(nonce(direction, seq)), additionalData: buf(ad), tagLength: 128 }, key, buf(plain))
  const out = new Uint8Array(SECURE_HEADER + sealed.byteLength)
  out.set(ad)
  out.set(new Uint8Array(sealed), SECURE_HEADER)
  return out
}

export function secureHeader(record: Uint8Array): { kind: number; seq: bigint } {
  if (record.length < SECURE_HEADER + TAG) throw new AuthError('secure record too short')
  if (record[0] < 1 || record[0] > 9) throw new AuthError(`unknown record kind ${record[0]}`)
  return { kind: record[0], seq: new DataView(record.buffer, record.byteOffset, record.byteLength).getBigUint64(1) }
}

export async function open(key: CryptoKey, direction: Direction, record: Uint8Array): Promise<{ kind: number; seq: bigint; plain: Uint8Array }> {
  const { kind, seq } = secureHeader(record)
  try {
    const plain = await subtle().decrypt(
      { name: 'AES-GCM', iv: buf(nonce(direction, seq)), additionalData: buf(record.subarray(0, SECURE_HEADER)), tagLength: 128 },
      key,
      buf(record.subarray(SECURE_HEADER)),
    )
    return { kind, seq, plain: new Uint8Array(plain) }
  } catch {
    throw new AuthError('secure record failed authentication')
  }
}

/**
 * One end of a session: seals what it sends with its own direction's key and
 * counter, opens what it receives with the peer's key and a replay window.
 * The browser is `side: 'b2h'`.
 */
export class SecureSession {
  private readonly sendKey: CryptoKey
  private readonly recvKey: CryptoKey
  private readonly side: Direction
  private readonly peer: Direction
  private next = 0n
  private readonly window = new ReplayWindow()

  constructor(keys: SessionKeys, side: Direction = 'b2h') {
    this.side = side
    this.peer = side === 'b2h' ? 'h2b' : 'b2h'
    this.sendKey = keys[side]
    this.recvKey = keys[this.peer]
  }

  seal(kind: number, payload: Uint8Array): Promise<Uint8Array> {
    const seq = this.next
    if (seq >= MAX_SEQ) throw new Error('sequence space exhausted; reconnect')
    this.next = seq + 1n
    return seal(this.sendKey, this.side, kind, seq, payload)
  }

  /** Opens a received record: its kind, seq and payload. */
  async open(record: Uint8Array): Promise<{ kind: number; seq: bigint; plain: Uint8Array }> {
    const { seq } = secureHeader(record)
    if (!this.window.check(seq)) throw new ReplayError(`replayed or stale record ${seq}`)
    const opened = await open(this.recvKey, this.peer, record)
    // Another record with this seq may have authenticated while we awaited.
    if (!this.window.check(seq)) throw new ReplayError(`replayed record ${seq}`)
    this.window.accept(seq)
    return opened
  }
}
