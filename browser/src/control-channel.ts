/** Sensitive control runs only on the reliable DataChannel belonging to the
 * verified WebRTC peer. DTLS encryption is supplied by that peer, not by this
 * framing. No HTTP fallback, queued commands or implicit retry. */
import type { SignedControl } from './session-control.js';
export const CONTROL_CHANNEL_LABEL = 'veynode-control-v1';
export const CONTROL_CHANNEL_PROTOCOL = 'veynode-control/v1';
export const CONTROL_FRAGMENT_BYTES = 16 * 1024;
export const CONTROL_MESSAGE_BYTES = 1024 * 1024;
const HEADER_BYTES = 17, HIGH_WATER = 64 * 1024;
export class ControlChannelError extends Error { constructor(message: string) { super(message); this.name = 'ControlChannelError'; } }
interface Options {
  isConfirmed: () => boolean;
  onFailure?: (error: Error) => void;
  requestTimeoutMs?: number;
  sendTimeoutMs?: number;
  assemblyTimeoutMs?: number;
}
interface Pending {
  sequence: number;
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  sent: boolean;
  received: number;
  bytes?: Uint8Array<ArrayBuffer>;
  timer: ReturnType<typeof setTimeout>;
  watch: ReturnType<typeof setInterval>;
  assembly?: ReturnType<typeof setTimeout>;
  removeAbort: () => void;
}
function requireValue(value: unknown, message: string): asserts value { if (!value) throw new ControlChannelError(message); }
function timeout(value: number | undefined, defaultValue: number, maximum: number): number {
  const result = value ?? defaultValue;
  requireValue(Number.isInteger(result) && result > 0 && result <= maximum, 'Invalid control timeout'); return result;
}
export class EncryptedControlChannel {
  private pending: Pending | undefined;
  private failure: Error | undefined;
  private lastSequence = 0;
  private readonly requestTimeout: number;
  private readonly sendTimeout: number;
  private readonly assemblyTimeout: number;
  constructor(private readonly channel: RTCDataChannel, private readonly options: Options) {
    this.requestTimeout = timeout(options.requestTimeoutMs, 25000, 30000);
    this.sendTimeout = timeout(options.sendTimeoutMs, 2000, 2000);
    this.assemblyTimeout = timeout(options.assemblyTimeoutMs, 5000, 5000);
    try {
      requireValue(channel.label === CONTROL_CHANNEL_LABEL && channel.protocol === CONTROL_CHANNEL_PROTOCOL && channel.ordered === true && channel.negotiated === false && channel.maxRetransmits === null && channel.maxPacketLifeTime === null, 'Control requires the exact reliable ordered channel');
      requireValue(channel.readyState === 'connecting' || channel.readyState === 'open', 'Control channel is not usable');
    } catch (error) { channel.close(); throw error; }
    channel.binaryType = 'arraybuffer';
    channel.bufferedAmountLowThreshold = HIGH_WATER / 2;
    channel.addEventListener('message', this.receive);
    channel.addEventListener('close', this.closed);
    channel.addEventListener('error', this.errored);
  }
  private closed = () => this.close('Control channel closed; session control is fenced');
  private errored = () => this.close('Control channel transport failed; session control is fenced');
  get fenced(): boolean { return this.failure !== undefined; }
  private authorized(): boolean { try { return this.options.isConfirmed(); } catch { return false; } }
  async request(signed: SignedControl, signal?: AbortSignal): Promise<unknown> {
    if (this.failure) throw this.failure;
    requireValue(this.authorized(), 'Confirmed signed session required for control');
    requireValue(!this.pending, 'Only one control request may be in flight');
    signal?.throwIfAborted();
    const sequence = signed?.request?.sequence;
    requireValue(Number.isSafeInteger(sequence) && sequence > this.lastSequence, 'Control sequence must increase');
    const text = JSON.stringify(signed);
    requireValue(text.length <= CONTROL_MESSAGE_BYTES, 'Control request too large');
    const bytes = new TextEncoder().encode(text);
    requireValue(bytes.length > 0 && bytes.length <= CONTROL_MESSAGE_BYTES, 'Control request too large');
    this.lastSequence = sequence;
    const result = new Promise<unknown>((resolve, reject) => {
      const abort = () => this.close('Control request cancelled; outcome may be unknown');
      const pending: Pending = {
        sequence, resolve, reject, sent: false, received: 0,
        timer: setTimeout(() => this.close('Control response deadline exceeded; outcome may be unknown'), this.requestTimeout),
        watch: setInterval(() => { if (!this.authorized()) this.close('Session authorization ended'); }, 20),
        removeAbort: () => signal?.removeEventListener('abort', abort),
      };
      this.pending = pending;
      signal?.addEventListener('abort', abort, { once: true });
      if (signal?.aborted) { abort(); return; }
      void this.send(bytes, pending).catch(error => this.close(error instanceof Error ? error : String(error)));
    });
    return result;
  }
  private async send(bytes: Uint8Array, pending: Pending): Promise<void> {
    const deadline = performance.now() + this.sendTimeout;
    for (let offset = 0; offset < bytes.length;) {
      const length = Math.min(CONTROL_FRAGMENT_BYTES - HEADER_BYTES, bytes.length - offset);
      for (;;) {
        if (this.failure) throw this.failure;
        requireValue(this.pending === pending && this.authorized(), 'Control request lost authorization');
        requireValue(performance.now() < deadline, 'Control send deadline exceeded');
        requireValue(this.channel.readyState === 'connecting' || this.channel.readyState === 'open', 'Control channel closed while sending');
        requireValue(Number.isSafeInteger(this.channel.bufferedAmount) && this.channel.bufferedAmount >= 0 && this.channel.bufferedAmount <= HIGH_WATER, 'Control send buffer exceeded bound');
        if (this.channel.readyState === 'open' && this.channel.bufferedAmount + HEADER_BYTES + length <= HIGH_WATER) break;
        await new Promise(resolve => setTimeout(resolve, 10));
      }
      const frame = new Uint8Array(HEADER_BYTES + length), header = new DataView(frame.buffer);
      header.setUint8(0, 1); header.setBigUint64(1, BigInt(pending.sequence)); header.setUint32(9, bytes.length); header.setUint32(13, offset);
      frame.set(bytes.subarray(offset, offset + length), HEADER_BYTES);
      this.channel.send(frame.buffer);
      offset += length;
    }
    pending.sent = true;
  }
  private receive = (event: MessageEvent) => {
    if (this.failure) return;
    try {
      const pending = this.pending;
      requireValue(pending && pending.sent && this.authorized(), 'Unsolicited or premature control response');
      requireValue(event.data instanceof ArrayBuffer, 'Control frames must be binary ArrayBuffers');
      const frame = new Uint8Array(event.data);
      requireValue(frame.length > HEADER_BYTES && frame.length <= CONTROL_FRAGMENT_BYTES, 'Invalid control fragment size');
      const header = new DataView(frame.buffer), sequence = header.getBigUint64(1), total = header.getUint32(9), offset = header.getUint32(13);
      requireValue(header.getUint8(0) === 1 && sequence === BigInt(pending.sequence), 'Control version or sequence substituted');
      requireValue(total > 0 && total <= CONTROL_MESSAGE_BYTES && offset === pending.received && offset + frame.length - HEADER_BYTES <= total, 'Invalid control fragment bounds/order');
      if (!pending.bytes) {
        pending.bytes = new Uint8Array(total);
        pending.assembly = setTimeout(() => this.close('Control response assembly deadline exceeded'), this.assemblyTimeout);
      }
      requireValue(pending.bytes.length === total, 'Control message length changed');
      pending.bytes.set(frame.subarray(HEADER_BYTES), offset); pending.received += frame.length - HEADER_BYTES;
      if (pending.received !== total) return;
      let value: Record<string, unknown>;
      try { value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(pending.bytes)) as Record<string, unknown>; }
      catch { throw new ControlChannelError('Malformed control response'); }
      requireValue(value !== null && typeof value === 'object' && !Array.isArray(value) && value.sequence === pending.sequence, 'Invalid control response envelope');
      const keys = Object.keys(value).sort();
      if (keys.join(',') === 'error,sequence') {
        requireValue(value.error === 'control rejected', 'Invalid control rejection envelope');
        throw new ControlChannelError('Control rejected; session control is fenced');
      }
      requireValue(keys.join(',') === 'result,sequence' && value.result !== null && typeof value.result === 'object' && !Array.isArray(value.result), 'Invalid control response fields');
      this.clearPending(); pending.resolve(value);
    } catch (error) { this.close(error instanceof Error ? error : String(error)); }
  };
  private clearPending() {
    const pending = this.pending; if (!pending) return;
    clearTimeout(pending.timer); clearTimeout(pending.assembly); clearInterval(pending.watch); pending.removeAbort(); this.pending = undefined;
  }
  close(reason: string | Error = 'Control session closed'): void {
    if (this.failure) return;
    const error = reason instanceof Error ? reason : new ControlChannelError(reason);
    this.failure = error; const pending = this.pending; this.clearPending();
    this.channel.removeEventListener('message', this.receive); this.channel.removeEventListener('close', this.closed); this.channel.removeEventListener('error', this.errored);
    try { this.channel.close(); } catch { /* local fence already prevents reuse */ }
    pending?.reject(error);
    try { this.options.onFailure?.(error); } catch { /* transport remains fenced if UI notification fails */ }
  }
}
