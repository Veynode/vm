// Browser side of the remote desktop handshake (plan §3.3). Host side:
// `crates/media-local/src/remote/handshake.rs`; signed bytes:
// `crates/session-auth/src/remote.rs` (domains `Veynode/remote-offer/v1`,
// `remote-answer/v1`, `remote-confirm/v1`). Every message is a plaintext
// handshake unit `0x07 | JSON` with a "t" field, sent before keys exist:
//
//   B→H hello     {proto, device_id, vm_id, scopes}
//   H→B challenge {challenge}                          session-auth Challenge
//   B→H offer     {offer: {intent, signature}}         device key signs the intent
//   H→B answer    {answer: {transcript, signature}}    host key signs the transcript
//   B→H confirm   {session_id, confirmation: {transcript_sha256, signature}}
//   H→B error     {code, message}                      at any step; the route ends
//
// The intent binds the challenge, our ephemeral ECDH key, the outer
// transport and the page origin. Record keys are HKDF(ECDH, salt = sha256 of
// the canonical transcript), so a gateway that swaps either ECDH key breaks
// a signature. The canonical encoding follows `browser/src/session-auth.ts`
// (Canon: domain\0, u32-length text, u64 integers, u32-count string lists).

import { handshakeUnit } from './proto'
import { deriveKeys, generateEphemeral, type Ephemeral, type SessionKeys } from './secure'

export const PROTOCOL = 1
export const CODECS = ['h264', 'h265'] as const
export type Codec = (typeof CODECS)[number]
export const SCOPES = ['execute', 'files', 'input', 'lifecycle', 'observe'] as const
export const MAX_SESSION_SECONDS = 8 * 3600
const MAX_SAFE = Number.MAX_SAFE_INTEGER

export interface Challenge {
  host_id: string
  device_id: string
  vm_id: string
  scopes: string[]
  grant_version: number
  instance_nonce: string
  nonce: string
  issued_at: number
  expires_at: number
}

export interface RemoteOfferIntent {
  challenge: Challenge
  request_id: string
  codecs: Codec[]
  browser_ecdh: string
  transport: 'wt' | 'wss'
  origin: string
}

export interface RemoteTranscript {
  intent: RemoteOfferIntent
  session_id: string
  host_ecdh: string
  codec: Codec
  expires_at: number
}

export interface Confirmation {
  transcript_sha256: string
  signature: string
}

/** The paired device key (`browser/src/session-auth.ts` DeviceIdentity fits). */
export interface DeviceSigner {
  readonly id: string
  readonly publicKey: string
  sign(bytes: Uint8Array): Promise<string>
}

export class HandshakeError extends Error {
  /** Close code from the host's `error` message (4001 for every refusal), or 0 for a local check. */
  readonly code: number
  constructor(message: string, code = 0) {
    super(message)
    this.name = 'HandshakeError'
    this.code = code
  }
}

function check(value: unknown, message: string): asserts value {
  if (!value) throw new HandshakeError(message)
}

// --- encoding ---------------------------------------------------------------

const encoder = new TextEncoder()

export function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')
}

export function unhex(value: unknown, length: number): Uint8Array<ArrayBuffer> {
  check(typeof value === 'string' && value.length === length * 2 && /^[0-9a-f]*$/.test(value), 'invalid lowercase hex')
  return Uint8Array.from({ length }, (_, i) => Number.parseInt(value.slice(i * 2, i * 2 + 2), 16))
}

export async function sha256(bytes: Uint8Array): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest('SHA-256', new Uint8Array(bytes))))
}

function utf8(value: string): Uint8Array {
  // A Rust String cannot hold an unpaired surrogate; never replace one silently.
  for (let i = 0; i < value.length; i++) {
    const c = value.charCodeAt(i)
    if (c >= 0xd800 && c <= 0xdbff) {
      const next = value.charCodeAt(++i)
      check(next >= 0xdc00 && next <= 0xdfff, 'unpaired UTF-16 surrogate')
    } else check(c < 0xdc00 || c > 0xdfff, 'unpaired UTF-16 surrogate')
  }
  return encoder.encode(value)
}

class Canon {
  private chunks: Uint8Array[]
  constructor(domain: string) {
    this.chunks = [encoder.encode(`Veynode/${domain}/v1\0`)]
  }
  text(value: string) {
    const bytes = utf8(value)
    const size = new Uint8Array(4)
    new DataView(size.buffer).setUint32(0, bytes.length)
    this.chunks.push(size, bytes)
  }
  int(value: number) {
    check(Number.isSafeInteger(value) && value >= 0, 'unsafe or invalid integer')
    const bytes = new Uint8Array(8)
    new DataView(bytes.buffer).setBigUint64(0, BigInt(value))
    this.chunks.push(bytes)
  }
  strings(values: readonly string[]) {
    const count = new Uint8Array(4)
    new DataView(count.buffer).setUint32(0, values.length)
    this.chunks.push(count)
    for (const v of values) this.text(v)
  }
  bytes(): Uint8Array<ArrayBuffer> {
    const out = new Uint8Array(this.chunks.reduce((n, c) => n + c.length, 0))
    let at = 0
    for (const c of this.chunks) {
      out.set(c, at)
      at += c.length
    }
    return out
  }
}

const identifier = (v: unknown) => check(typeof v === 'string' && /^[A-Za-z0-9_.-]{1,128}$/.test(v), 'invalid identifier')
const sortedUnique = (values: readonly string[], allowed: readonly string[]) =>
  values.length > 0 && values.every((v, i) => allowed.includes(v) && (i === 0 || values[i - 1] < v))

function ecdhKey(value: unknown) {
  check(unhex(value, 65)[0] === 4, 'ECDH key must be an uncompressed P-256 point')
}

function validateChallenge(c: Challenge) {
  unhex(c.host_id, 32)
  unhex(c.device_id, 32)
  identifier(c.vm_id)
  check(Array.isArray(c.scopes) && c.scopes.length <= SCOPES.length && sortedUnique(c.scopes, SCOPES), 'scopes must be known, sorted and unique')
  unhex(c.instance_nonce, 32)
  unhex(c.nonce, 32)
  for (const n of [c.grant_version, c.issued_at, c.expires_at]) check(Number.isSafeInteger(n) && n >= 0, 'integer out of range')
  check(c.grant_version >= 1, 'integer out of range')
  check(c.expires_at > c.issued_at && c.expires_at - c.issued_at <= 60, 'invalid challenge lifetime')
}

function challengeFields(c: Challenge, canon: Canon) {
  canon.text(c.host_id)
  canon.text(c.device_id)
  canon.text(c.vm_id)
  canon.strings(c.scopes)
  canon.int(c.grant_version)
  canon.text(c.instance_nonce)
  canon.text(c.nonce)
  canon.int(c.issued_at)
  canon.int(c.expires_at)
}

function intentFields(i: RemoteOfferIntent, canon: Canon) {
  challengeFields(i.challenge, canon)
  canon.text(i.request_id)
  canon.strings(i.codecs)
  canon.text(i.browser_ecdh)
  canon.text(i.transport)
  canon.text(i.origin)
}

function validateIntent(i: RemoteOfferIntent) {
  validateChallenge(i.challenge)
  identifier(i.request_id)
  check(Array.isArray(i.codecs) && sortedUnique(i.codecs, CODECS), 'codecs must be known, sorted and unique')
  ecdhKey(i.browser_ecdh)
  check(i.transport === 'wt' || i.transport === 'wss', 'invalid transport')
  check(
    typeof i.origin === 'string' && i.origin.length <= 256 && /^https:\/\/[A-Za-z0-9.:[\]-]+$/.test(i.origin),
    'origin must be https://host[:port]',
  )
}

/** `RemoteOfferIntent::canonical`: what the device key signs. */
export function remoteOfferBytes(intent: RemoteOfferIntent): Uint8Array<ArrayBuffer> {
  validateIntent(intent)
  const c = new Canon('remote-offer')
  intentFields(intent, c)
  return c.bytes()
}

/** `RemoteTranscript::canonical`: what the host key signs; its sha256 is the confirm digest and the HKDF salt. */
export function remoteAnswerBytes(t: RemoteTranscript): Uint8Array<ArrayBuffer> {
  validateIntent(t.intent)
  identifier(t.session_id)
  ecdhKey(t.host_ecdh)
  check(t.intent.codecs.includes(t.codec), 'codec was not offered')
  const issued = t.intent.challenge.issued_at
  check(
    Number.isSafeInteger(t.expires_at) && t.expires_at > issued && t.expires_at - issued <= MAX_SESSION_SECONDS && t.expires_at <= MAX_SAFE,
    'invalid session lifetime',
  )
  const c = new Canon('remote-answer')
  intentFields(t.intent, c)
  c.text(t.session_id)
  c.text(t.host_ecdh)
  c.text(t.codec)
  c.int(t.expires_at)
  return c.bytes()
}

/** `remote_confirmation_bytes`: what the device key signs to confirm. */
export function remoteConfirmationBytes(digest: string): Uint8Array<ArrayBuffer> {
  unhex(digest, 32)
  const c = new Canon('remote-confirm')
  c.text(digest)
  return c.bytes()
}

export async function identityId(publicKey: string): Promise<string> {
  const raw = unhex(publicKey, 65)
  const domain = encoder.encode('Veynode/identity/v1\0')
  const bytes = new Uint8Array(domain.length + 65)
  bytes.set(domain)
  bytes.set(raw, domain.length)
  return sha256(bytes)
}

async function verify(publicKey: string, bytes: Uint8Array<ArrayBuffer>, signature: string) {
  const raw = unhex(publicKey, 65)
  check(raw[0] === 4, 'host key must be an uncompressed P-256 point')
  let key: CryptoKey
  try {
    key = await crypto.subtle.importKey('raw', raw, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify'])
  } catch {
    throw new HandshakeError('invalid host public key')
  }
  check(await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, unhex(signature, 64), bytes), 'invalid host signature')
}

const sameBytes = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((x, i) => x === b[i])

// --- the client -------------------------------------------------------------

export interface HandshakeOptions {
  identity: DeviceSigner
  /** The host key pinned at pairing (uncompressed SEC1 hex). */
  hostPublicKey: string
  vmId: string
  /** Sorted, unique session-auth scopes; without `input` the host refuses kinds 3 and 9. */
  scopes: string[]
  /** Codecs this browser decodes; sorted before signing. */
  codecs: Codec[]
  /** The outer transport actually in use; the host checks it against what the gateway saw. */
  transport: 'wt' | 'wss'
  /** The page origin, e.g. `location.origin`. */
  origin: string
  requestId?: string
  /** Injected for tests. */
  ephemeral?: () => Promise<Ephemeral>
}

export interface Established {
  sessionId: string
  codec: Codec
  /** Unix seconds; the host closes the session then. */
  expiresAt: number
  keys: SessionKeys
  transcript: RemoteTranscript
  transcriptSha256: string
}

export type HandshakeStep = { send?: Uint8Array; established?: Established }

type Stage = 'start' | 'hello' | 'offered' | 'done'

/**
 * Feed it every handshake message (`classifyUnit(...).message`); send what
 * it returns. Errors (local checks or the host's refusal) are HandshakeError
 * and end the attempt.
 */
export class RemoteHandshake {
  private stage: Stage = 'start'
  private intent: RemoteOfferIntent | undefined
  private ephemeral: Ephemeral | undefined
  private readonly options: HandshakeOptions

  constructor(options: HandshakeOptions) {
    check(sortedUnique(options.scopes, SCOPES), 'scopes must be known, sorted and unique')
    this.options = { ...options, codecs: [...new Set(options.codecs)].sort() }
  }

  hello(): Uint8Array {
    check(this.stage === 'start', 'hello already sent')
    this.stage = 'hello'
    const { identity, vmId, scopes } = this.options
    return handshakeUnit({ t: 'hello', proto: PROTOCOL, device_id: identity.id, vm_id: vmId, scopes })
  }

  async receive(message: any): Promise<HandshakeStep> {
    if (message?.t === 'error') {
      this.stage = 'done'
      throw new HandshakeError(String(message.message ?? 'refused'), Number(message.code) || 0)
    }
    if (this.stage === 'hello' && message?.t === 'challenge') return this.challenge(message.challenge)
    if (this.stage === 'offered' && message?.t === 'answer') return this.answer(message.answer)
    this.stage = 'done'
    throw new HandshakeError(`unexpected handshake message ${String(message?.t)}`)
  }

  private async challenge(challenge: Challenge): Promise<HandshakeStep> {
    this.stage = 'done'
    const { identity, hostPublicKey, vmId, scopes } = this.options
    validateChallenge(challenge)
    check(challenge.host_id === (await identityId(hostPublicKey)), 'challenge from another host')
    check(challenge.device_id === identity.id, 'challenge for another device')
    check(challenge.vm_id === vmId, 'challenge for another VM')
    check(challenge.scopes.join(',') === scopes.join(','), 'challenge scopes differ from those asked')
    // No local clock check: a phone's clock may be off, and the host
    // enforces the window and consumes the nonce itself.
    const ephemeral = await (this.options.ephemeral ?? generateEphemeral)()
    const intent: RemoteOfferIntent = {
      challenge,
      request_id: this.options.requestId ?? crypto.randomUUID(),
      codecs: this.options.codecs,
      browser_ecdh: hex(ephemeral.publicKey),
      transport: this.options.transport,
      origin: this.options.origin,
    }
    const signature = await identity.sign(remoteOfferBytes(intent))
    this.intent = intent
    this.ephemeral = ephemeral
    this.stage = 'offered'
    return { send: handshakeUnit({ t: 'offer', offer: { intent, signature } }) }
  }

  private async answer(answer: { transcript: RemoteTranscript; signature: string }): Promise<HandshakeStep> {
    this.stage = 'done'
    const intent = this.intent!
    const t = answer?.transcript
    check(t && typeof t === 'object', 'malformed answer')
    // Same canonical bytes ⇔ the same intent, field for field.
    check(sameBytes(remoteOfferBytes(t.intent), remoteOfferBytes(intent)), 'offer intent substituted')
    const bytes = remoteAnswerBytes(t)
    await verify(this.options.hostPublicKey, bytes, answer.signature)
    const digest = await sha256(bytes)
    const keys = await deriveKeys(this.ephemeral!.privateKey, unhex(t.host_ecdh, 65), unhex(digest, 32))
    const signature = await this.options.identity.sign(remoteConfirmationBytes(digest))
    return {
      send: handshakeUnit({ t: 'confirm', session_id: t.session_id, confirmation: { transcript_sha256: digest, signature } }),
      established: { sessionId: t.session_id, codec: t.codec, expiresAt: t.expires_at, keys, transcript: t, transcriptSha256: digest },
    }
  }
}
