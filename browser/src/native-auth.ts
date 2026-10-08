/** Authenticated loopback receiver signaling. Caller pins the host key through an
 * independent trusted channel and installs only the verified answer returned here.
 * Browser code delivery remains trusted; this module does not make HTTP E2EE. */
import { BrowserIdentityStore, DeviceIdentity, SessionAuthError, SCOPES, identityId, pairingBytes, sha256, randomNonce, verifyGrant, signOffer, confirmAnswer, signClose, type Scope, type Pairing, type SignedGrant, type Challenge, type SignedAnswer, type AnswerTranscript, type Confirmation } from './session-auth.js';
import { signControl, validateControlResponse, CONTROL_MAX_RESPONSE_BYTES, type ControlAction, type ControlResponse } from './session-control.js';
export interface VerifiedAnswer { answer: { type: 'answer'; sdp: string; session_id: string }; transcript: AnswerTranscript; confirmation: Confirmation }
export interface Configuration { pinnedHostPublicKey: string; vmId: string; databaseName?: string; scopes?: Scope[] }
function ensure(value: unknown, message: string): asserts value { if (!value) throw new SessionAuthError(message); }
function fixedConfiguration(options: Configuration): Configuration & { scopes: Scope[] } {
  ensure(options.scopes === undefined || Array.isArray(options.scopes), 'Scopes must be an array');
  const scopes = [...(options.scopes ?? ['observe'])];
  ensure(scopes.length > 0 && scopes.length <= SCOPES.length && scopes.every((scope, i) => SCOPES.includes(scope) && (i === 0 || scopes[i - 1]! < scope)), 'Scopes must be known, sorted and unique');
  return { ...options, scopes };
}
export class AuthenticatedReceiver {
  private grant?: SignedGrant;
  private session: { transcript: AnswerTranscript; confirmation: Confirmation; confirmed: boolean; closing: boolean; sequence: number } | undefined;
  private busy = false;
  private disposed = false;
  private constructor(readonly identity: DeviceIdentity, readonly pinnedHostPublicKey: string, readonly vmId: string, private readonly scopes: Scope[], private readonly store?: BrowserIdentityStore) {}
  static async open(options: Configuration): Promise<AuthenticatedReceiver> {
    options = fixedConfiguration(options);
    await identityId(options.pinnedHostPublicKey);
    const store = await BrowserIdentityStore.open(options.databaseName ?? 'veynode-session-auth-receiver');
    try { const { identity } = await store.loadOrCreate(); return await AuthenticatedReceiver.fromIdentity(identity, options, store); }
    catch (error) { store.close(); throw error; }
  }
  // Also useful for applications already holding a persisted identity handle.
  static async fromIdentity(identity: DeviceIdentity, options: Configuration, store?: BrowserIdentityStore): Promise<AuthenticatedReceiver> {
    const configuration = fixedConfiguration(options);
    await identityId(configuration.pinnedHostPublicKey);
    const receiver = new AuthenticatedReceiver(identity, configuration.pinnedHostPublicKey, configuration.vmId, configuration.scopes, store);
    await receiver.pairingRequest(); return receiver;
  }
  get deviceId(): string { return this.identity.id; }
  get publicKey(): string { return this.identity.publicKey; }
  private usable() { ensure(!this.disposed, 'Receiver is disposed'); }
  async pairingRequest(): Promise<{ pairing: Pairing; digest: string; deviceId: string }> {
    this.usable(); const pairing: Pairing = { host_public_key: this.pinnedHostPublicKey, device_public_key: this.identity.publicKey, nonce: randomNonce(), vm_id: this.vmId, scopes: [...this.scopes] };
    return { pairing, digest: await sha256(await pairingBytes(pairing)), deviceId: this.identity.id };
  }
  async acceptGrant(signed: SignedGrant): Promise<void> {
    this.usable(); ensure(!this.busy && !this.session, 'Cannot change grant during a session');
    this.busy = true;
    try {
      const copy = structuredClone(signed);
      await verifyGrant(copy, this.pinnedHostPublicKey, { device_public_key: this.identity.publicKey, vm_id: this.vmId, scopes: [...this.scopes] });
      this.grant = copy;
    } finally { this.busy = false; }
  }
  private async post(path: string, body: unknown, signal?: AbortSignal, maximumBytes = 512 * 1024): Promise<unknown> {
    const deadline = AbortSignal.timeout(15000);
    const combined = signal ? AbortSignal.any([signal, deadline]) : deadline;
    const response = await fetch(path, { method: 'POST', credentials: 'omit', redirect: 'error', cache: 'no-store', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body), signal: combined });
    ensure(response.ok, `Authenticated signaling rejected (${response.status})`);
    const reader = response.body?.getReader(); ensure(reader, 'Missing signaling response');
    const chunks: Uint8Array[] = []; let count = 0;
    try { for (;;) { const part = await reader.read(); if (part.done) break; count += part.value.length; ensure(count <= maximumBytes, 'Signaling response too large'); chunks.push(part.value); } }
    catch (error) { await reader.cancel().catch(() => {}); throw error; }
    finally { reader.releaseLock(); }
    const bytes = new Uint8Array(count); let offset = 0; for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
    return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
  }
  async offer(exactSDP: string, requestID: string, codec: 'h264' | 'h265', signal?: AbortSignal): Promise<VerifiedAnswer> {
    this.usable(); ensure(this.grant, 'An independently verified signed grant is required'); ensure(!this.busy && !this.session, 'Receiver already has a pending or active session'); this.busy = true;
    try {
      const challenge = await this.post('/auth/challenge', { device_id: this.deviceId, vm_id: this.vmId, scopes: [...this.scopes] }, signal) as Challenge;
      const signed = await signOffer(this.identity, this.grant, this.pinnedHostPublicKey, challenge, exactSDP, { vm_id: this.vmId, scopes: [...this.scopes], request_id: requestID, codec, now: Math.floor(Date.now() / 1000) });
      const response = await this.post('/auth/offer', { signed_offer: signed, sdp: exactSDP }, signal) as { type?: unknown; sdp?: unknown; signed_answer?: SignedAnswer; session_id?: unknown };
      ensure(response && response.type === 'answer' && typeof response.sdp === 'string' && response.signed_answer, 'Missing signed SDP answer');
      const verified = await confirmAnswer(this.identity, response.signed_answer, this.pinnedHostPublicKey, signed.intent, response.sdp, Math.floor(Date.now() / 1000));
      if (response.session_id !== undefined) ensure(response.session_id === verified.transcript.session_id, 'Outer session ID substituted');
      this.session = { ...structuredClone(verified), confirmed: false, closing: false, sequence: 0 };
      return { answer: { type: 'answer', sdp: response.sdp, session_id: verified.transcript.session_id }, ...structuredClone(verified) };
    } finally { this.busy = false; }
  }
  async confirm(signal?: AbortSignal): Promise<void> {
    this.usable(); ensure(!this.busy && this.session && !this.session.closing, 'No verified session to confirm'); if (this.session.confirmed) return; this.busy = true;
    try { const result = await this.post('/auth/confirm', { session_id: this.session.transcript.session_id, confirmation: this.session.confirmation }, signal) as { confirmed?: unknown }; ensure(result?.confirmed === true, 'Session confirmation not acknowledged'); this.session.confirmed = true; }
    finally { this.busy = false; }
  }
  async signedClose() { this.usable(); ensure(!this.busy && this.session, 'No verified session to close'); return signClose(this.identity, structuredClone(this.session.transcript)); }
  /** No retries/reacquisition: an uncertain response may follow a dispatched
   * input. Sequence is consumed before signing and remains consumed on failure. */
  async control(action: ControlAction, signal?: AbortSignal): Promise<ControlResponse> {
    this.usable(); ensure(!this.busy, 'Another session operation is pending');
    ensure(this.session?.confirmed && !this.session.closing, 'A confirmed open session is required for control');
    ensure(this.session.sequence < Number.MAX_SAFE_INTEGER, 'Control sequence exhausted');
    const session = this.session, snapshot = structuredClone(action), sequence = ++session.sequence;
    this.busy = true;
    try {
      const signed = await signControl(this.identity, session.transcript, snapshot, { sequence, now: Math.floor(Date.now() / 1000) });
      return validateControlResponse(await this.post('/auth/control', signed, signal, CONTROL_MAX_RESPONSE_BYTES), sequence, snapshot);
    } finally { this.busy = false; }
  }
  async close(signal?: AbortSignal): Promise<void> {
    this.usable(); ensure(!this.busy, 'Signaling is pending; await its completion before closing'); if (!this.session) return;
    this.busy = true; this.session.closing = true;
    try { const signed = await signClose(this.identity, this.session.transcript); const result = await this.post('/auth/close', signed, signal) as { closed?: unknown }; ensure(result?.closed === true, 'Session cleanup not acknowledged'); this.session = undefined; }
    finally { this.busy = false; }
  }
  async unpair(): Promise<void> {
    this.usable(); ensure(!this.busy && !this.session, 'Close the session before local unpair'); ensure(this.store, 'Identity is owned by another store'); await this.store.unpair(this.identity); this.dispose();
  }
  dispose(): void { ensure(!this.busy && !this.session, 'Close the session before disposing'); this.store?.close(); this.disposed = true; }
}
