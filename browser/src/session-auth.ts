/** Veynode session-auth v1. No network, SDP installation or media authority here.
 * Wire contract: crates/session-auth/src/lib.rs. ECDSA uses WebCrypto's 64-byte
 * P1363 r||s, not ASN.1 DER: https://www.w3.org/TR/2017/REC-WebCryptoAPI-20170126/#ecdsa-operations
 */
export const SCOPES = ['execute', 'files', 'input', 'lifecycle', 'observe'] as const;
export type Scope = typeof SCOPES[number];
export interface Pairing { host_public_key: string; device_public_key: string; nonce: string; vm_id: string; scopes: Scope[] }
export interface Grant { host_id: string; device_id: string; device_public_key: string; vm_id: string; scopes: Scope[]; version: number }
export interface SignedGrant { grant: Grant; signature: string }
export interface Challenge { host_id: string; device_id: string; vm_id: string; scopes: Scope[]; grant_version: number; instance_nonce: string; nonce: string; issued_at: number; expires_at: number }
export interface SdpBinding { sha256: string; fingerprint_sha256: string }
export interface OfferIntent { challenge: Challenge; request_id: string; codec: 'h264' | 'h265'; offer: SdpBinding }
export interface SignedOffer { intent: OfferIntent; signature: string }
export const OFFER_REJECTION_CODES = ['guest_capture_unavailable', 'guest_display_inactive', 'guest_display_asleep'] as const;
export interface OfferRejection { intent: OfferIntent; code: typeof OFFER_REJECTION_CODES[number]; outcome: 'not_admitted'; version: 1 }
export interface SignedOfferRejection { rejection: OfferRejection; signature: string }
export interface AnswerTranscript { intent: OfferIntent; session_id: string; answer: SdpBinding }
export interface SignedAnswer { transcript: AnswerTranscript; signature: string }
export interface Confirmation { transcript_sha256: string; signature: string }
export interface SignedClose { session_id: string; transcript_sha256: string; signature: string }
export class SessionAuthError extends Error { constructor(message: string) { super(message); this.name = 'SessionAuthError'; } }
const encoder = new TextEncoder();
const keyAlgorithm = { name: 'ECDSA', namedCurve: 'P-256' } as const;
const signatureAlgorithm = { name: 'ECDSA', hash: 'SHA-256' } as const;
function requireCondition(value: unknown, message: string): asserts value { if (!value) throw new SessionAuthError(message); }
function exact(value: unknown, fields: string[]): asserts value is Record<string, unknown> {
  requireCondition(value !== null && typeof value === 'object' && !Array.isArray(value), 'Expected object');
  const keys = Object.keys(value);
  requireCondition(keys.length === fields.length && keys.every(key => fields.includes(key)), 'Unknown or missing fields');
}
function text(value: unknown): asserts value is string { requireCondition(typeof value === 'string', 'Expected string'); }
function integer(value: unknown, minimum = 0): asserts value is number { requireCondition(typeof value === 'number' && Number.isSafeInteger(value) && value >= minimum, 'Unsafe or invalid integer'); }
function identifier(value: unknown): asserts value is string { text(value); requireCondition(/^[A-Za-z0-9_.-]{1,128}$/.test(value), 'Invalid identifier'); }
function scopes(value: unknown): asserts value is Scope[] {
  requireCondition(Array.isArray(value) && value.length > 0 && value.length <= SCOPES.length, 'Invalid scopes');
  requireCondition(value.every((v, i) => SCOPES.includes(v) && (i === 0 || value[i - 1] < v)), 'Scopes must be known, sorted and unique');
}
export function hex(bytes: Uint8Array): string { return Array.from(bytes, b => b.toString(16).padStart(2, '0')).join(''); }
export function unhex(value: unknown, length: number): Uint8Array<ArrayBuffer> {
  text(value); requireCondition(value.length === length * 2 && /^[0-9a-f]+$/.test(value), 'Invalid lowercase hex');
  return Uint8Array.from({ length }, (_, i) => Number.parseInt(value.slice(i * 2, i * 2 + 2), 16));
}
function utf8(value: string): Uint8Array<ArrayBuffer> {
  // Rust String cannot contain unpaired UTF-16 surrogates; never silently replace.
  for (let i = 0; i < value.length; i++) {
    const c = value.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) { const next = value.charCodeAt(++i); requireCondition(next >= 0xdc00 && next <= 0xdfff, 'Unpaired UTF-16 surrogate'); }
    else requireCondition(c < 0xdc00 || c > 0xdfff, 'Unpaired UTF-16 surrogate');
  }
  return encoder.encode(value);
}
export async function sha256(bytes: Uint8Array): Promise<string> { return hex(new Uint8Array(await crypto.subtle.digest('SHA-256', new Uint8Array(bytes)))); }
export function randomNonce(): string { return hex(crypto.getRandomValues(new Uint8Array(32))); }
async function importPublic(publicKey: string): Promise<CryptoKey> {
  const raw = unhex(publicKey, 65); requireCondition(raw[0] === 4, 'Only uncompressed SEC1 P-256 keys accepted');
  try { return await crypto.subtle.importKey('raw', raw, keyAlgorithm, true, ['verify']); }
  catch { throw new SessionAuthError('Invalid P-256 public point'); }
}
export async function identityId(publicKey: string): Promise<string> {
  await importPublic(publicKey);
  const domain = encoder.encode('Veynode/identity/v1\0');
  const bytes = new Uint8Array(domain.length + 65); bytes.set(domain); bytes.set(unhex(publicKey, 65), domain.length);
  return sha256(bytes);
}
export async function verifySignature(publicKey: string, bytes: Uint8Array, signature: string): Promise<void> {
  const key = await importPublic(publicKey);
  requireCondition(await crypto.subtle.verify(signatureAlgorithm, key, unhex(signature, 64), new Uint8Array(bytes)), 'Invalid signature');
}
class Canon {
  private chunks: Uint8Array<ArrayBuffer>[];
  constructor(domain: string) { this.chunks = [encoder.encode(`Veynode/${domain}/v1\0`)]; }
  text(value: string) { const bytes = utf8(value); const size = new Uint8Array(4); new DataView(size.buffer).setUint32(0, bytes.length); this.chunks.push(size, bytes); }
  int(value: number) { integer(value); const bytes = new Uint8Array(8); new DataView(bytes.buffer).setBigUint64(0, BigInt(value)); this.chunks.push(bytes); }
  scopes(values: Scope[]) { const count = new Uint8Array(4); new DataView(count.buffer).setUint32(0, values.length); this.chunks.push(count); values.forEach(value => this.text(value)); }
  bytes(): Uint8Array<ArrayBuffer> { const result = new Uint8Array(this.chunks.reduce((n, x) => n + x.length, 0)); let offset = 0; for (const chunk of this.chunks) { result.set(chunk, offset); offset += chunk.length; } return result; }
}
export async function pairingBytes(value: Pairing): Promise<Uint8Array<ArrayBuffer>> {
  exact(value, ['host_public_key', 'device_public_key', 'nonce', 'vm_id', 'scopes']);
  await Promise.all([importPublic(value.host_public_key), importPublic(value.device_public_key)]); unhex(value.nonce, 32); identifier(value.vm_id); scopes(value.scopes);
  const c = new Canon('pairing'); c.text(value.host_public_key); c.text(value.device_public_key); c.text(value.nonce); c.text(value.vm_id); c.scopes(value.scopes); return c.bytes();
}
export async function grantBytes(value: Grant): Promise<Uint8Array<ArrayBuffer>> {
  exact(value, ['host_id', 'device_id', 'device_public_key', 'vm_id', 'scopes', 'version']);
  unhex(value.host_id, 32); requireCondition(await identityId(value.device_public_key) === value.device_id, 'Device identity mismatch'); identifier(value.vm_id); scopes(value.scopes); integer(value.version, 1);
  const c = new Canon('grant'); c.text(value.host_id); c.text(value.device_id); c.text(value.device_public_key); c.text(value.vm_id); c.scopes(value.scopes); c.int(value.version); return c.bytes();
}
function challengeFields(value: Challenge, c: Canon) {
  exact(value, ['host_id', 'device_id', 'vm_id', 'scopes', 'grant_version', 'instance_nonce', 'nonce', 'issued_at', 'expires_at']);
  unhex(value.host_id, 32); unhex(value.device_id, 32); identifier(value.vm_id); scopes(value.scopes); integer(value.grant_version, 1); unhex(value.instance_nonce, 32); unhex(value.nonce, 32); integer(value.issued_at); integer(value.expires_at);
  requireCondition(value.expires_at > value.issued_at && value.expires_at - value.issued_at <= 60, 'Invalid challenge lifetime');
  c.text(value.host_id); c.text(value.device_id); c.text(value.vm_id); c.scopes(value.scopes); c.int(value.grant_version); c.text(value.instance_nonce); c.text(value.nonce); c.int(value.issued_at); c.int(value.expires_at);
}
function bindingFields(value: SdpBinding, c: Canon) { exact(value, ['sha256', 'fingerprint_sha256']); unhex(value.sha256, 32); unhex(value.fingerprint_sha256, 32); c.text(value.sha256); c.text(value.fingerprint_sha256); }
function intentFields(value: OfferIntent, c: Canon) {
  exact(value, ['challenge', 'request_id', 'codec', 'offer']); challengeFields(value.challenge, c); identifier(value.request_id); requireCondition(value.codec === 'h264' || value.codec === 'h265', 'Invalid codec'); c.text(value.request_id); c.text(value.codec); bindingFields(value.offer, c);
}
export function offerBytes(value: OfferIntent): Uint8Array<ArrayBuffer> { const c = new Canon('offer'); intentFields(value, c); return c.bytes(); }
export function offerRejectionBytes(value: OfferRejection): Uint8Array<ArrayBuffer> {
  exact(value, ['intent', 'code', 'outcome', 'version']);
  requireCondition(value.version === 1 && value.outcome === 'not_admitted' && OFFER_REJECTION_CODES.includes(value.code), 'Invalid offer rejection');
  const c = new Canon('offer-rejection'); c.int(value.version); intentFields(value.intent, c); c.text(value.code); c.text(value.outcome); return c.bytes();
}
/** This terminal receipt proves only non-admission of the exact current offer.
 * It grants no session authority, and may arrive after that challenge expires. */
export async function verifyOfferRejection(signed: SignedOfferRejection, pinnedHostPublic: string, expectedIntent: OfferIntent): Promise<OfferRejection> {
  const value = structuredClone(signed); exact(value, ['rejection', 'signature']); const bytes = offerRejectionBytes(value.rejection);
  requireCondition(sameBytes(offerBytes(expectedIntent), offerBytes(value.rejection.intent)), 'Offer rejection intent substituted');
  requireCondition(value.rejection.intent.challenge.host_id === await identityId(pinnedHostPublic), 'Pinned host mismatch');
  await verifySignature(pinnedHostPublic, bytes, value.signature); return value.rejection;
}
export function answerBytes(value: AnswerTranscript): Uint8Array<ArrayBuffer> { exact(value, ['intent', 'session_id', 'answer']); const c = new Canon('answer'); intentFields(value.intent, c); identifier(value.session_id); c.text(value.session_id); bindingFields(value.answer, c); return c.bytes(); }
export function confirmationBytes(digest: string): Uint8Array<ArrayBuffer> { unhex(digest, 32); const c = new Canon('confirm'); c.text(digest); return c.bytes(); }
export function closeBytes(sessionId: string, digest: string): Uint8Array<ArrayBuffer> { identifier(sessionId); unhex(digest, 32); const c = new Canon('close'); c.text(sessionId); c.text(digest); return c.bytes(); }
export async function sdpBinding(sdp: string): Promise<SdpBinding> {
  text(sdp); const bytes = utf8(sdp); requireCondition(bytes.length > 0 && bytes.length <= 128 * 1024 && !sdp.includes('\0'), 'Invalid SDP size/content');
  let fingerprint: string | undefined; let sessionFingerprint = false; let media = 0; let mediaWithFingerprint = 0; let currentFingerprint = false;
  for (let line of sdp.split('\n')) {
    if (line.endsWith('\r')) line = line.slice(0, -1);
    requireCondition(!line.includes('\r'), 'Invalid SDP line ending');
    if (line.startsWith('m=')) { if (media && currentFingerprint) mediaWithFingerprint++; media++; currentFingerprint = false; }
    if (line.startsWith('a=fingerprint:')) {
      const fields = line.slice(14).split(' ');
      requireCondition(fields.length === 2 && fields[0] === 'sha-256' && /^(?:[0-9a-fA-F]{2}:){31}[0-9a-fA-F]{2}$/.test(fields[1] ?? ''), 'Unsupported or malformed DTLS fingerprint');
      const normalized = fields[1]!.replaceAll(':', '').toLowerCase();
      requireCondition(fingerprint === undefined || fingerprint === normalized, 'Conflicting DTLS fingerprints'); fingerprint = normalized;
      if (media === 0) sessionFingerprint = true; else currentFingerprint = true;
    }
  }
  if (media && currentFingerprint) mediaWithFingerprint++;
  requireCondition(media > 0 && (sessionFingerprint || media === mediaWithFingerprint) && fingerprint !== undefined, 'Unbound media fingerprint');
  return { sha256: await sha256(bytes), fingerprint_sha256: fingerprint };
}
function sameBytes(a: Uint8Array, b: Uint8Array): boolean { return a.length === b.length && a.every((byte, i) => byte === b[i]); }
function current(challenge: Challenge, now: number) { integer(now); requireCondition(now >= challenge.issued_at && now < challenge.expires_at, 'Challenge expired or not yet valid'); }
export async function verifyGrant(signed: SignedGrant, pinnedHostPublic: string, expected: { device_public_key: string; vm_id: string; scopes: Scope[]; version?: number }): Promise<Grant> {
  const value = structuredClone(signed); exact(value, ['grant', 'signature']); const bytes = await grantBytes(value.grant);
  requireCondition(value.grant.host_id === await identityId(pinnedHostPublic), 'Pinned host mismatch');
  requireCondition(value.grant.device_public_key === expected.device_public_key && value.grant.vm_id === expected.vm_id && JSON.stringify(value.grant.scopes) === JSON.stringify(expected.scopes), 'Approved pairing grant substituted');
  if (expected.version !== undefined) requireCondition(value.grant.version === expected.version, 'Grant version substituted');
  await verifySignature(pinnedHostPublic, bytes, value.signature); return value.grant;
}
export async function verifyOffer(signed: SignedOffer, grant: Grant, expectedChallenge: Challenge, exactSdp: string, now: number): Promise<OfferIntent> {
  const value = structuredClone(signed); exact(value, ['intent', 'signature']); const bytes = offerBytes(value.intent);
  requireCondition(sameBytes(offerBytes({ ...value.intent, challenge: expectedChallenge }), bytes), 'Challenge substituted'); current(value.intent.challenge, now);
  await grantBytes(grant); const c = value.intent.challenge;
  requireCondition(c.host_id === grant.host_id && c.device_id === grant.device_id && c.vm_id === grant.vm_id && c.grant_version === grant.version && c.scopes.every(scope => grant.scopes.includes(scope)), 'Grant or scope mismatch');
  const binding = await sdpBinding(exactSdp); requireCondition(binding.sha256 === value.intent.offer.sha256 && binding.fingerprint_sha256 === value.intent.offer.fingerprint_sha256, 'Offer SDP substituted');
  await verifySignature(grant.device_public_key, bytes, value.signature); return value.intent;
}
export async function verifyAnswer(signed: SignedAnswer, pinnedHostPublic: string, expectedIntent: OfferIntent, exactAnswerSdp: string, now: number): Promise<AnswerTranscript> {
  const value = structuredClone(signed); exact(value, ['transcript', 'signature']); const bytes = answerBytes(value.transcript);
  requireCondition(sameBytes(offerBytes(expectedIntent), offerBytes(value.transcript.intent)), 'Offer intent substituted');
  requireCondition(value.transcript.intent.challenge.host_id === await identityId(pinnedHostPublic), 'Pinned host mismatch'); current(value.transcript.intent.challenge, now);
  const binding = await sdpBinding(exactAnswerSdp); requireCondition(binding.sha256 === value.transcript.answer.sha256 && binding.fingerprint_sha256 === value.transcript.answer.fingerprint_sha256, 'Answer SDP substituted');
  await verifySignature(pinnedHostPublic, bytes, value.signature); return value.transcript;
}
export async function verifyConfirmation(value: Confirmation, transcript: AnswerTranscript, devicePublic: string, now: number): Promise<void> {
  exact(value, ['transcript_sha256', 'signature']); const digest = await sha256(answerBytes(transcript)); current(transcript.intent.challenge, now);
  requireCondition(transcript.intent.challenge.device_id === await identityId(devicePublic), 'Device substituted'); requireCondition(value.transcript_sha256 === digest, 'Transcript substituted');
  await verifySignature(devicePublic, confirmationBytes(digest), value.signature);
}
export async function signClose(identity: DeviceIdentity, transcript: AnswerTranscript): Promise<SignedClose> {
  requireCondition(transcript.intent.challenge.device_id === identity.id, 'Close belongs to another device');
  const transcript_sha256 = await sha256(answerBytes(transcript));
  return { session_id: transcript.session_id, transcript_sha256, signature: await identity.sign(closeBytes(transcript.session_id, transcript_sha256)) };
}
export async function verifyClose(value: SignedClose, transcript: AnswerTranscript, devicePublic: string): Promise<void> {
  exact(value, ['session_id', 'transcript_sha256', 'signature']);
  requireCondition(value.session_id === transcript.session_id && value.transcript_sha256 === await sha256(answerBytes(transcript)), 'Close session/transcript substituted');
  requireCondition(transcript.intent.challenge.device_id === await identityId(devicePublic), 'Close device substituted');
  await verifySignature(devicePublic, closeBytes(value.session_id, value.transcript_sha256), value.signature);
}

/** Call with the independently pinned host key and locally requested VM/scopes.
 * The host still must consume this exact challenge once in its SessionGate. */
export async function signOffer(identity: DeviceIdentity, signedGrant: SignedGrant, pinnedHostPublic: string, challenge: Challenge, exactOfferSdp: string, options: { vm_id: string; scopes: Scope[]; request_id: string; codec: 'h264' | 'h265'; now: number }): Promise<SignedOffer> {
  const grant = await verifyGrant(signedGrant, pinnedHostPublic, { device_public_key: identity.publicKey, vm_id: options.vm_id, scopes: signedGrant.grant.scopes });
  const intent: OfferIntent = { challenge: structuredClone(challenge), request_id: options.request_id, codec: options.codec, offer: await sdpBinding(exactOfferSdp) };
  const bytes = offerBytes(intent); scopes(options.scopes); current(intent.challenge, options.now);
  requireCondition(intent.challenge.host_id === grant.host_id && intent.challenge.device_id === identity.id && intent.challenge.vm_id === options.vm_id && intent.challenge.grant_version === grant.version && JSON.stringify(intent.challenge.scopes) === JSON.stringify(options.scopes) && options.scopes.every(scope => grant.scopes.includes(scope)), 'Requested challenge binding substituted');
  return { intent, signature: await identity.sign(bytes) };
}

/** Verify before calling setRemoteDescription or sending any sensitive data. */
export async function confirmAnswer(identity: DeviceIdentity, signed: SignedAnswer, pinnedHostPublic: string, expectedIntent: OfferIntent, exactAnswerSdp: string, now: number): Promise<{ transcript: AnswerTranscript; confirmation: Confirmation }> {
  const transcript = await verifyAnswer(signed, pinnedHostPublic, expectedIntent, exactAnswerSdp, now);
  requireCondition(transcript.intent.challenge.device_id === identity.id, 'Answer belongs to another device');
  const transcript_sha256 = await sha256(answerBytes(transcript));
  return { transcript, confirmation: { transcript_sha256, signature: await identity.sign(confirmationBytes(transcript_sha256)) } };
}

/** A non-extractable identity handle; same-origin code can use keys, so trusted
 * browser-code distribution and host-side grant revocation remain mandatory. */
export class DeviceIdentity {
  private revoked = false;
  private constructor(private readonly keys: CryptoKeyPair, readonly publicKey: string, readonly id: string, private readonly stillStored?: () => Promise<boolean>) {}
  static async fromKeyPair(keys: CryptoKeyPair, stillStored?: () => Promise<boolean>): Promise<DeviceIdentity> {
    requireCondition(keys.privateKey.type === 'private' && !keys.privateKey.extractable && keys.privateKey.algorithm.name === 'ECDSA' && (keys.privateKey.algorithm as EcKeyAlgorithm).namedCurve === 'P-256' && keys.privateKey.usages.length === 1 && keys.privateKey.usages[0] === 'sign', 'Unsafe stored private key');
    requireCondition(keys.publicKey.type === 'public' && keys.publicKey.algorithm.name === 'ECDSA' && (keys.publicKey.algorithm as EcKeyAlgorithm).namedCurve === 'P-256', 'Invalid stored public key');
    const publicKey = hex(new Uint8Array(await crypto.subtle.exportKey('raw', keys.publicKey)));
    const proof = crypto.getRandomValues(new Uint8Array(32)); const signature = await crypto.subtle.sign(signatureAlgorithm, keys.privateKey, proof);
    requireCondition(await crypto.subtle.verify(signatureAlgorithm, keys.publicKey, signature, proof), 'Stored identity key pair mismatch');
    return new DeviceIdentity(keys, publicKey, await identityId(publicKey), stillStored);
  }
  static async generate(): Promise<DeviceIdentity> { return DeviceIdentity.fromKeyPair(await crypto.subtle.generateKey(keyAlgorithm, false, ['sign', 'verify'])); }
  get privateKeyExtractable(): boolean { return this.keys.privateKey.extractable; }
  async assertPrivateExportRejected(): Promise<void> {
    let exported = false;
    // Node22 names the same non-extractability rejection InvalidAccessException;
    // both runtimes must actually reject, never merely report extractable=false.
    try { await crypto.subtle.exportKey('pkcs8', this.keys.privateKey); exported = true; } catch (error) { requireCondition(error instanceof DOMException && ['InvalidAccessError', 'InvalidAccessException'].includes(error.name), 'Unexpected private export failure'); }
    requireCondition(!exported, 'Private key unexpectedly exportable');
  }
  async sign(bytes: Uint8Array): Promise<string> {
    requireCondition(!this.revoked && (!this.stillStored || await this.stillStored()), 'Identity unpaired or replaced');
    const signature = new Uint8Array(await crypto.subtle.sign(signatureAlgorithm, this.keys.privateKey, new Uint8Array(bytes)));
    requireCondition(signature.length === 64, 'Unexpected ECDSA signature encoding'); return hex(signature);
  }
  revokeHandle() { this.revoked = true; }
}
interface StoredIdentity { schema: 1; keys: CryptoKeyPair; public_key: string; identity_id: string }
export class BrowserIdentityStore {
  private constructor(private readonly db: IDBDatabase, private readonly name: string) {}
  static async open(databaseName: string, identityName = 'device'): Promise<BrowserIdentityStore> {
    requireCondition(/^veynode-session-auth(?:-[a-z0-9-]+)?$/.test(databaseName), 'Use an explicit session-auth database namespace'); identifier(identityName);
    requireCondition(typeof indexedDB !== 'undefined', 'IndexedDB unavailable');
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = indexedDB.open(databaseName, 1); let rejected = false;
      request.onupgradeneeded = () => request.result.createObjectStore('identities');
      request.onerror = () => { rejected = true; reject(new SessionAuthError('Identity database unavailable')); };
      request.onblocked = () => { rejected = true; reject(new SessionAuthError('Identity database upgrade blocked')); };
      request.onsuccess = () => { if (rejected) request.result.close(); else resolve(request.result); };
    });
    db.onversionchange = () => db.close(); return new BrowserIdentityStore(db, identityName);
  }
  private read(): Promise<StoredIdentity | undefined> {
    return new Promise((resolve, reject) => { const tx = this.db.transaction('identities', 'readonly'); const request = tx.objectStore('identities').get(this.name); let value: StoredIdentity | undefined; request.onsuccess = () => { value = request.result as StoredIdentity | undefined; }; tx.oncomplete = () => resolve(value); tx.onabort = tx.onerror = () => reject(new SessionAuthError('Identity read failed')); });
  }
  async loadOrCreate(): Promise<{ identity: DeviceIdentity; created: boolean }> {
    let stored = await this.read(); let created = false;
    if (!stored) {
      const keys = await crypto.subtle.generateKey(keyAlgorithm, false, ['sign', 'verify']);
      const publicKey = hex(new Uint8Array(await crypto.subtle.exportKey('raw', keys.publicKey)));
      const candidate: StoredIdentity = { schema: 1, keys, public_key: publicKey, identity_id: await identityId(publicKey) };
      stored = await new Promise<StoredIdentity>((resolve, reject) => {
        const tx = this.db.transaction('identities', 'readwrite'); const store = tx.objectStore('identities'); const get = store.get(this.name); let winner: StoredIdentity;
        get.onsuccess = () => { if (get.result) winner = get.result as StoredIdentity; else { winner = candidate; store.add(candidate, this.name); created = true; } };
        tx.oncomplete = () => resolve(winner); tx.onabort = tx.onerror = () => reject(new SessionAuthError('Identity creation/structured clone failed'));
      });
    }
    requireCondition(stored.schema === 1, 'Unsupported stored identity schema');
    const expected = stored.identity_id;
    const identity = await DeviceIdentity.fromKeyPair(stored.keys, async () => (await this.read())?.identity_id === expected);
    requireCondition(identity.id === stored.identity_id && identity.publicKey === stored.public_key, 'Stored identity metadata mismatch');
    return { identity, created };
  }
  async unpair(identity: DeviceIdentity): Promise<void> {
    // Delete only this explicitly named identity; no localStorage or database wipe.
    await new Promise<void>((resolve, reject) => {
      const tx = this.db.transaction('identities', 'readwrite'); const store = tx.objectStore('identities'); const get = store.get(this.name);
      get.onsuccess = () => { if (get.result && (get.result as StoredIdentity).identity_id !== identity.id) { tx.abort(); return; } store.delete(this.name); };
      tx.oncomplete = () => resolve(); tx.onabort = tx.onerror = () => reject(new SessionAuthError('Unpair failed or identity already replaced'));
    });
    identity.revokeHandle();
  }
  close() { this.db.close(); }
}
