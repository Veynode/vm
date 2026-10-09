// Synthetic ordered DataChannel; no claim of DTLS/network validation.
export function frames(sequence, value, fragmentBytes = 16384) {
  const bytes = typeof value === 'string' ? Buffer.from(value) : Buffer.from(JSON.stringify(value)); const result = [];
  for (let offset = 0; offset < bytes.length; offset += fragmentBytes - 17) {
    const payload = bytes.subarray(offset, offset + fragmentBytes - 17), frame = Buffer.alloc(17 + payload.length);
    frame[0] = 1; frame.writeBigUInt64BE(BigInt(sequence), 1); frame.writeUInt32BE(bytes.length, 9); frame.writeUInt32BE(offset, 13); payload.copy(frame, 17);
    result.push(Uint8Array.from(frame).buffer);
  }
  return result;
}
export class FakeChannel extends EventTarget {
  label = 'veynode-control-v1'; protocol = 'veynode-control/v1'; ordered = true; negotiated = false;
  maxRetransmits = null; maxPacketLifeTime = null; readyState = 'open'; binaryType = 'blob'; bufferedAmount = 0; bufferedAmountLowThreshold = 0;
  sent = []; incoming = []; constructor(handle) { super(); this.handle = handle; }
  send(data) {
    if (this.readyState !== 'open') throw Error('closed');
    const bytes = Buffer.from(data); this.sent.push(Buffer.from(bytes));
    if (!this.handle) return;
    if (bytes.length > 16384 || bytes.length <= 17 || bytes[0] !== 1) throw Error('bad browser frame');
    const sequence = Number(bytes.readBigUInt64BE(1)), total = bytes.readUInt32BE(9), offset = bytes.readUInt32BE(13);
    if (offset !== this.incoming.reduce((n, x) => n + x.length, 0)) throw Error('bad browser offset');
    this.incoming.push(Buffer.from(bytes.subarray(17)));
    if (offset + bytes.length - 17 === total) {
      const body = JSON.parse(Buffer.concat(this.incoming).toString()); this.incoming = [];
      queueMicrotask(() => { void Promise.resolve().then(() => this.handle(body)).then(value => { if (this.readyState === 'open') for (const frame of frames(sequence, value)) this.emit(frame); }).catch(error => { this.hostError = error; this.close(); }); });
    }
  }
  emit(data) { this.dispatchEvent(new MessageEvent('message', { data })); }
  close() { if (this.readyState === 'closed') return; this.readyState = 'closed'; this.dispatchEvent(new Event('close')); }
}
