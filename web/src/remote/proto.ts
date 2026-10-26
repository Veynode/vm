// Remote desktop wire format, browser side (plan §3). The Rust half,
// `crates/remote-proto/src/record.rs` and `frag.rs`, is the reference; every
// integer is big-endian, like the local relay's
// records (`web/desktop.mjs` RecordReader) that kinds 1 and 2 stay
// compatible with. `vectors.json` beside this file pins the exact bytes.
//
// Layers, outermost first:
//   outer transport  WebTransport / WebSocket (TLS ends at vm-gateway)
//   secure record    u8 kind | u64 seq | AES-128-GCM(plain record)   (secure.ts)
//   plain record     u32 len | u8 kind | payload, len = 1 + payload length
//
// The plain record is what gets encrypted, so its kind is checked against
// the secure record's (which is in the AAD) after opening. On the datagram
// video path a sealed kind 8 record carries a fragment instead, and the
// fragments rebuild the video_v2 payload. The gateway's route header is
// between gateway and host only; the browser never sees it.

export const KIND = {
  event: 1,
  video: 2,
  input: 3,
  control: 4,
  ping: 5,
  stats: 6,
  hello: 7,
  videoV2: 8,
  pointer: 9,
} as const
export type Kind = (typeof KIND)[keyof typeof KIND]

/** Largest payload: the local encoders' 4 MiB access-unit ceiling plus headers (record.rs MAX_PAYLOAD). */
export const MAX_PAYLOAD = 4 * 1024 * 1024 + 64
/** Largest JSON payload (every kind but 2, 8 and 9). */
export const MAX_JSON = 64 * 1024

export const known = (kind: number) => kind >= KIND.event && kind <= KIND.pointer
const binary = (kind: number) => kind === KIND.video || kind === KIND.videoV2 || kind === KIND.pointer

/** WebSocket / WebTransport close codes (record.rs `close`, as veynode/core's account mesh). */
export const CLOSE = {
  unauthorized: 4001,
  replaced: 4002,
  revoked: 4003,
  rateLimited: 4008,
  invalid: 4009,
  retry: 4503,
} as const

export interface PlainRecord {
  kind: number
  payload: Uint8Array
}

export function encodeRecord(kind: number, payload: Uint8Array): Uint8Array {
  if (!known(kind)) throw new Error(`unknown record kind ${kind}`)
  if (payload.length > MAX_PAYLOAD) throw new Error('record outside bound')
  const out = new Uint8Array(5 + payload.length)
  new DataView(out.buffer).setUint32(0, payload.length + 1)
  out[4] = kind
  out.set(payload, 5)
  return out
}

/** One complete record (a WebSocket message, a decrypted secure record). */
export function decodeRecord(bytes: Uint8Array): PlainRecord {
  if (bytes.length < 5) throw new Error('record too short')
  const length = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0)
  if (length < 1 || length - 1 > MAX_PAYLOAD) throw new Error('record outside bound')
  if (bytes.length !== 4 + length) throw new Error('record length mismatch')
  const kind = bytes[4]
  if (!known(kind)) throw new Error(`unknown record kind ${kind}`)
  if (!binary(kind) && length - 1 > MAX_JSON) throw new Error('JSON record too large')
  return { kind, payload: bytes.subarray(5) }
}

/** Splits a byte stream into length-prefixed units (`u32 len | len bytes`). */
export class LengthReader {
  private buffer: Uint8Array = new Uint8Array(0)
  private readonly max: number

  // No parameter properties in this folder: Node's type stripping, which the
  // tests run under, does not accept them.
  constructor(max = MAX_PAYLOAD + 64) {
    this.max = max
  }

  push(chunk: Uint8Array): Uint8Array[] {
    if (this.buffer.length) {
      const joined = new Uint8Array(this.buffer.length + chunk.length)
      joined.set(this.buffer)
      joined.set(chunk, this.buffer.length)
      this.buffer = joined
    } else this.buffer = chunk
    const out: Uint8Array[] = []
    while (this.buffer.length >= 4) {
      const length = new DataView(this.buffer.buffer, this.buffer.byteOffset).getUint32(0)
      if (length < 1 || length > this.max) throw new Error('unit outside bound')
      if (this.buffer.length < 4 + length) break
      out.push(this.buffer.slice(4, 4 + length))
      this.buffer = this.buffer.subarray(4 + length)
    }
    return out
  }
}

/** `u32 len | bytes`, for secure records on a WebTransport stream. */
export function lengthPrefixed(bytes: Uint8Array): Uint8Array {
  const out = new Uint8Array(4 + bytes.length)
  new DataView(out.buffer).setUint32(0, bytes.length)
  out.set(bytes, 4)
  return out
}

const encoder = new TextEncoder()
const decoder = new TextDecoder()

/** Kinds 1, 3, 4, 5 and 6 carry one UTF-8 JSON value as their payload. */
export function jsonPayload(value: unknown): Uint8Array {
  return encoder.encode(JSON.stringify(value))
}

export function parseJson(payload: Uint8Array): any {
  return JSON.parse(decoder.decode(payload))
}

// --- kind 8, video_v2 -------------------------------------------------------

export const VIDEO_FLAG_KEY = 1
export const VIDEO_FLAG_SPS_CHANGED = 2
export const VIEW_LEN = 12
export const VIDEO_V2_HEADER = 29 + VIEW_LEN

/**
 * The part of the source screen a frame shows, in source pixels (record.rs
 * `View`). Full screen is `x = y = 0, w = src_w, h = src_h`.
 */
export interface View {
  x: number
  y: number
  w: number
  h: number
  srcW: number
  srcH: number
}

export interface VideoV2 {
  epoch: number
  frameSeq: number
  key: boolean
  spsChanged: boolean
  ptsUs: bigint
  /** Capture to encode-complete. */
  encodeUs: number
  /** Time the access unit waited in the host's queue. */
  hostQueueUs: number
  /** Highest input seq the host had applied when this frame was captured. */
  inputAckSeq: number
  view: View
  /** Annex B access unit. */
  au: Uint8Array
}

/**
 * `u32 epoch, u32 frame_seq, u8 flags, u64 pts_us, u32 encode_us,
 * u32 host_queue_us, u32 input_ack_seq, u16 x, u16 y, u16 w, u16 h,
 * u16 src_w, u16 src_h, Annex B`.
 */
export function encodeVideoV2(v: VideoV2): Uint8Array {
  const out = new Uint8Array(VIDEO_V2_HEADER + v.au.length)
  const view = new DataView(out.buffer)
  view.setUint32(0, v.epoch)
  view.setUint32(4, v.frameSeq)
  out[8] = (v.key ? VIDEO_FLAG_KEY : 0) | (v.spsChanged ? VIDEO_FLAG_SPS_CHANGED : 0)
  view.setBigUint64(9, v.ptsUs)
  view.setUint32(17, v.encodeUs)
  view.setUint32(21, v.hostQueueUs)
  view.setUint32(25, v.inputAckSeq)
  const { x, y, w, h, srcW, srcH } = v.view
  ;[x, y, w, h, srcW, srcH].forEach((n, i) => view.setUint16(29 + 2 * i, n))
  out.set(v.au, VIDEO_V2_HEADER)
  return out
}

export function decodeVideoV2(payload: Uint8Array): VideoV2 {
  if (payload.length < VIDEO_V2_HEADER) throw new Error('video_v2 header too short')
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength)
  const flags = payload[8]
  const u16 = (i: number) => view.getUint16(29 + 2 * i)
  return {
    epoch: view.getUint32(0),
    frameSeq: view.getUint32(4),
    key: (flags & VIDEO_FLAG_KEY) !== 0,
    spsChanged: (flags & VIDEO_FLAG_SPS_CHANGED) !== 0,
    ptsUs: view.getBigUint64(9),
    encodeUs: view.getUint32(17),
    hostQueueUs: view.getUint32(21),
    inputAckSeq: view.getUint32(25),
    view: { x: u16(0), y: u16(1), w: u16(2), h: u16(3), srcW: u16(4), srcH: u16(5) },
    au: payload.subarray(VIDEO_V2_HEADER),
  }
}

/** Kind 2 (v1, the local relay's): `u64 pts_us, u8 key, Annex B`. */
export function decodeVideoV1(payload: Uint8Array): { ptsUs: bigint; key: boolean; au: Uint8Array } {
  if (payload.length < 9) throw new Error('video header too short')
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength)
  return { ptsUs: view.getBigUint64(0), key: payload[8] === 1, au: payload.subarray(9) }
}

// --- kind 9, pointer --------------------------------------------------------

export interface Pointer {
  seq: number
  x: number
  y: number
  w: number
  h: number
}

/** `u32 seq, i16 x, i16 y, u16 w, u16 h`. */
export function encodePointer(p: Pointer): Uint8Array {
  const out = new Uint8Array(12)
  const view = new DataView(out.buffer)
  view.setUint32(0, p.seq)
  view.setInt16(4, p.x)
  view.setInt16(6, p.y)
  view.setUint16(8, p.w)
  view.setUint16(10, p.h)
  return out
}

export function decodePointer(payload: Uint8Array): Pointer {
  if (payload.length !== 12) throw new Error('pointer record must be 12 bytes')
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength)
  return { seq: view.getUint32(0), x: view.getInt16(4), y: view.getInt16(6), w: view.getUint16(8), h: view.getUint16(10) }
}

// --- datagram fragments -----------------------------------------------------

export const FRAGMENT_HEADER = 12
/** Fragment plaintext (header + data) per datagram, before the 9-byte record header and 16-byte tag (frag.rs MAX_FRAGMENT). */
export const MAX_FRAGMENT = 1100
export const MAX_FRAGMENT_DATA = MAX_FRAGMENT - FRAGMENT_HEADER

export interface Fragment {
  epoch: number
  frameSeq: number
  idx: number
  count: number
  data: Uint8Array
}

/** `u32 epoch, u32 frame_seq, u16 idx, u16 count, bytes`, sealed as kind 8 (inside the ciphertext). */
export function encodeFragment(f: Fragment): Uint8Array {
  const out = new Uint8Array(FRAGMENT_HEADER + f.data.length)
  const view = new DataView(out.buffer)
  view.setUint32(0, f.epoch)
  view.setUint32(4, f.frameSeq)
  view.setUint16(8, f.idx)
  view.setUint16(10, f.count)
  out.set(f.data, FRAGMENT_HEADER)
  return out
}

export function decodeFragment(bytes: Uint8Array): Fragment {
  if (bytes.length <= FRAGMENT_HEADER || bytes.length > MAX_FRAGMENT) throw new Error('bad fragment size')
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const f = { epoch: view.getUint32(0), frameSeq: view.getUint32(4), idx: view.getUint16(8), count: view.getUint16(10) }
  if (f.count < 1 || f.idx >= f.count) throw new Error('fragment index outside count')
  return { ...f, data: bytes.subarray(FRAGMENT_HEADER) }
}

/** Splits a video_v2 payload (header included) into fragment plaintexts; they concatenate back to it. */
export function fragmentPayload(payload: Uint8Array, epoch: number, frameSeq: number): Uint8Array[] {
  if (!payload.length) throw new Error('empty frame')
  const count = Math.ceil(payload.length / MAX_FRAGMENT_DATA)
  if (count > 0xffff) throw new Error('frame too large for datagrams')
  const out: Uint8Array[] = []
  for (let idx = 0; idx < count; idx++)
    out.push(encodeFragment({ epoch, frameSeq, idx, count, data: payload.subarray(idx * MAX_FRAGMENT_DATA, (idx + 1) * MAX_FRAGMENT_DATA) }))
  return out
}

// --- reliable-path units ----------------------------------------------------

/**
 * What one unit on the reliable path is (WT bidi `u32 len | unit`, one WS
 * message per unit), by its first byte: `0x07 | JSON` a plaintext handshake
 * message (only before keys exist), `0xff | JSON` a gateway notice
 * (unauthenticated, display only), 1..9 otherwise a secure record. WT uni
 * streams and datagrams carry only secure records.
 */
export const UNIT_HANDSHAKE = 0x07
export const UNIT_GATEWAY = 0xff

export type Unit =
  | { type: 'handshake'; message: any }
  | { type: 'gateway'; notice: any }
  | { type: 'record'; bytes: Uint8Array }
  | { type: 'invalid'; reason: string }

export function classifyUnit(unit: Uint8Array): Unit {
  const first = unit[0]
  if (first === undefined) return { type: 'invalid', reason: 'empty unit' }
  const json = (type: 'handshake' | 'gateway'): Unit => {
    if (unit.length - 1 > MAX_JSON) return { type: 'invalid', reason: 'JSON unit too large' }
    try {
      const value = JSON.parse(decoder.decode(unit.subarray(1)))
      return type === 'handshake' ? { type, message: value } : { type, notice: value }
    } catch {
      return { type: 'invalid', reason: 'malformed JSON unit' }
    }
  }
  if (first === UNIT_HANDSHAKE) return json('handshake')
  if (first === UNIT_GATEWAY) return json('gateway')
  if (known(first)) return { type: 'record', bytes: unit }
  return { type: 'invalid', reason: `unknown unit type ${first}` }
}

export function handshakeUnit(message: unknown): Uint8Array {
  const body = encoder.encode(JSON.stringify(message))
  const out = new Uint8Array(1 + body.length)
  out[0] = UNIT_HANDSHAKE
  out.set(body, 1)
  return out
}
