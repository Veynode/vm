/** Physical-console control v1. Canonical bytes match Rust session-auth/control.rs.
 * Signatures authenticate requests; they do not encrypt HTTP, prove Guest input
 * delivery, or bind an RTP frame to a screenshot UUID. */
import { fileActionFields, validateFileResult, type FileAction } from './session-files.js';
import { DeviceIdentity, SessionAuthError, answerBytes, sha256, unhex, verifySignature, identityId, type AnswerTranscript } from './session-auth.js';

export interface FrameAnchor { frame_id: string; source_generation: number; width: number; height: number }
type Button = 'left' | 'middle' | 'right';
export type ControlAction =
  | { kind: 'file'; operation: FileAction }
  | { kind: 'acquire' }
  | { kind: 'release'; epoch: number }
  | { kind: 'observe_frame' }
  | { kind: 'click'; epoch: number; frame: FrameAnchor; x: number; y: number; button: Button }
  | { kind: 'drag'; epoch: number; frame: FrameAnchor; x: number; y: number; end_x: number; end_y: number; button: Button }
  | { kind: 'scroll'; epoch: number; frame: FrameAnchor; x: number; y: number; direction: 'up' | 'down' | 'left' | 'right'; ticks: number }
  | { kind: 'key'; epoch: number; frame: FrameAnchor; keys: string[] };
export interface ControlRequest { session_id: string; transcript_sha256: string; sequence: number; expires_at: number; action: ControlAction }
export interface SignedControl { request: ControlRequest; signature: string }
export interface ControlResponse { sequence: number; result: Record<string, unknown> }
export const CONTROL_MAX_RESPONSE_BYTES = 1024 * 1024;
export const SCREENSHOT_MAX_BYTES = 700 * 1024;
const encoder = new TextEncoder();
function ensure(value: unknown, message: string): asserts value { if (!value) throw new SessionAuthError(message); }
function exact(value: unknown, fields: string[]): asserts value is Record<string, unknown> {
  ensure(value !== null && typeof value === 'object' && !Array.isArray(value), 'Control value must be an object');
  const keys = Object.keys(value); ensure(keys.length === fields.length && keys.every(key => fields.includes(key)), 'Unknown or missing control fields');
}
function integer(value: unknown, minimum = 0, maximum = Number.MAX_SAFE_INTEGER): asserts value is number {
  ensure(typeof value === 'number' && Number.isSafeInteger(value) && value >= minimum && value <= maximum, 'Invalid control integer');
}
function text(value: unknown, expression: RegExp, message: string): asserts value is string { ensure(typeof value === 'string' && expression.test(value), message); }
class Canon {
  private chunks: Uint8Array<ArrayBuffer>[] = [encoder.encode('Veynode/control/v1\0')];
  text(value: string) { const bytes = encoder.encode(value); const size = new Uint8Array(4); new DataView(size.buffer).setUint32(0, bytes.length); this.chunks.push(size, bytes); }
  int(value: number) { integer(value); const bytes = new Uint8Array(8); new DataView(bytes.buffer).setBigUint64(0, BigInt(value)); this.chunks.push(bytes); }
  strings(values: string[]) { const count = new Uint8Array(4); new DataView(count.buffer).setUint32(0, values.length); this.chunks.push(count); values.forEach(value => this.text(value)); }
  bytes(): Uint8Array<ArrayBuffer> { const result = new Uint8Array(this.chunks.reduce((n, part) => n + part.length, 0)); let offset = 0; for (const part of this.chunks) { result.set(part, offset); offset += part.length; } return result; }
}
function validateFrame(frame: FrameAnchor) {
  exact(frame, ['frame_id', 'source_generation', 'width', 'height']);
  text(frame.frame_id, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/, 'Invalid screenshot frame ID');
  integer(frame.source_generation); integer(frame.width, 1, 8192); integer(frame.height, 1, 8192);
}
function frameFields(frame: FrameAnchor, c: Canon) { validateFrame(frame); c.text(frame.frame_id); c.int(frame.source_generation); c.int(frame.width); c.int(frame.height); }
function point(frame: FrameAnchor, x: unknown, y: unknown) { integer(x, 0, frame.width - 1); integer(y, 0, frame.height - 1); }
function actionFields(action: ControlAction, c: Canon) {
  ensure(action !== null && typeof action === 'object', 'Invalid control action');
  const kind = action.kind;
  if (kind === 'file') { exact(action, ['kind', 'operation']); c.text(kind); fileActionFields(action.operation, c); return; }
  if (kind === 'acquire' || kind === 'observe_frame') { exact(action, ['kind']); c.text(kind); return; }
  if (kind === 'release') { exact(action, ['kind', 'epoch']); integer(action.epoch, 1); c.text(kind); c.int(action.epoch); return; }
  const common = ['kind', 'epoch', 'frame'];
  if (kind === 'click') exact(action, [...common, 'x', 'y', 'button']);
  else if (kind === 'drag') exact(action, [...common, 'x', 'y', 'end_x', 'end_y', 'button']);
  else if (kind === 'scroll') exact(action, [...common, 'x', 'y', 'direction', 'ticks']);
  else if (kind === 'key') exact(action, [...common, 'keys']);
  else throw new SessionAuthError('Unsupported control action');
  integer(action.epoch, 1); c.text(kind); c.int(action.epoch); frameFields(action.frame, c);
  if (action.kind === 'key') {
    ensure(Array.isArray(action.keys) && action.keys.length >= 1 && action.keys.length <= 8 && new Set(action.keys).size === action.keys.length, 'Invalid or duplicate physical keys');
    for (const key of action.keys) text(key, /^[a-z0-9_]{1,16}$/, 'Invalid physical qcode'); c.strings(action.keys); return;
  }
  point(action.frame, action.x, action.y); c.int(action.x); c.int(action.y);
  if (action.kind === 'drag') { point(action.frame, action.end_x, action.end_y); c.int(action.end_x); c.int(action.end_y); }
  if (action.kind === 'scroll') { ensure(['up', 'down', 'left', 'right'].includes(action.direction), 'Invalid scroll direction'); integer(action.ticks, 1, 20); c.text(action.direction); c.int(action.ticks); }
  else { ensure(['left', 'middle', 'right'].includes(action.button), 'Invalid mouse button'); c.text(action.button); }
}
export function requiredControlScope(action: ControlAction): 'observe' | 'input' | 'files' { return action.kind === 'file' ? 'files' : action.kind === 'observe_frame' ? 'observe' : 'input'; }
export function controlBytes(request: ControlRequest): Uint8Array<ArrayBuffer> {
  exact(request, ['session_id', 'transcript_sha256', 'sequence', 'expires_at', 'action']);
  text(request.session_id, /^[A-Za-z0-9_.-]{1,128}$/, 'Invalid control session ID'); unhex(request.transcript_sha256, 32);
  integer(request.sequence, 1); integer(request.expires_at);
  const c = new Canon(); c.text(request.session_id); c.text(request.transcript_sha256); c.int(request.sequence); c.int(request.expires_at); actionFields(request.action, c); return c.bytes();
}
function current(request: ControlRequest, now: number) { integer(now); ensure(request.expires_at > now && request.expires_at - now <= 5, 'Control request expired or too far in future'); }
/** Caller must already verify and confirm the transcript. This signer does not
 * itself establish a session or acquire a lease, and never retries a request. */
export async function signControl(identity: DeviceIdentity, transcript: AnswerTranscript, action: ControlAction, options: { sequence: number; now: number; expires_at?: number }): Promise<SignedControl> {
  const copy = structuredClone(transcript), snapshot = structuredClone(action), settings = { ...options };
  ensure(copy.intent.challenge.device_id === identity.id, 'Control belongs to another device');
  const request: ControlRequest = { session_id: copy.session_id, transcript_sha256: await sha256(answerBytes(copy)), sequence: settings.sequence, expires_at: settings.expires_at ?? settings.now + 5, action: snapshot };
  const bytes = controlBytes(request); current(request, settings.now);
  ensure(copy.intent.challenge.scopes.includes(requiredControlScope(snapshot)), 'Control scope not granted');
  return { request, signature: await identity.sign(bytes) };
}
/** Cryptographic fixture/peer verification only; replay state belongs to the
 * host session gate and is not supplied by this stateless helper. */
export async function verifyControl(signed: SignedControl, transcript: AnswerTranscript, devicePublic: string, now: number): Promise<void> {
  const value = structuredClone(signed), copy = structuredClone(transcript); exact(value, ['request', 'signature']);
  const bytes = controlBytes(value.request); current(value.request, now);
  ensure(value.request.session_id === copy.session_id && value.request.transcript_sha256 === await sha256(answerBytes(copy)), 'Control session/transcript substituted');
  ensure(copy.intent.challenge.device_id === await identityId(devicePublic), 'Control device substituted');
  ensure(copy.intent.challenge.scopes.includes(requiredControlScope(value.request.action)), 'Control scope not granted');
  await verifySignature(devicePublic, bytes, value.signature);
}
/** Extract only the server-issued screenshot anchor. The caller must actually
 * display that image for coordinate input; an RTP video frame has no such ID. */
export function screenshotAnchor(result: Record<string, unknown>): FrameAnchor {
  ensure(result.input_anchor_available === undefined || result.input_anchor_available === true, 'Screenshot has no usable input anchor');
  const frame = { frame_id: result.frame_id, source_generation: result.source_generation, width: result.width, height: result.height } as FrameAnchor;
  validateFrame(frame); return frame;
}
export function validateControlResponse(value: unknown, sequence: number, action: ControlAction): ControlResponse {
  exact(value, ['sequence', 'result']); integer(value.sequence, 1); ensure(value.sequence === sequence, 'Control response sequence substituted');
  ensure(value.result !== null && typeof value.result === 'object' && !Array.isArray(value.result), 'Invalid control result');
  const result = value.result as Record<string, unknown>;
  if (action.kind === 'file') { validateFileResult(result, action.operation); } else if (action.kind === 'observe_frame') {
    screenshotAnchor(result); screenshotBytes(result);
  } else if (action.kind === 'acquire') { integer(result.epoch, 1); ensure(result.expires_in_seconds === 60, 'Invalid lease lifetime'); }
  else if (action.kind === 'release') { ensure(result.released === true, 'Control release not acknowledged'); }
  else { ensure(result.dispatched === true && result.guest_delivery_confirmed === false, 'Input dispatch not acknowledged'); }
  return { sequence, result };
}

/** Structural validation only; image decoder acceptance and declared dimensions
 * are checked by the page before the image can become an input anchor. */
function screenshotBytes(result: Record<string, unknown>): Uint8Array<ArrayBuffer> {
  integer(result.byte_length, 1, SCREENSHOT_MAX_BYTES);
  ensure(result.mime_type === 'image/png' || result.mime_type === 'image/jpeg', 'Unsupported screenshot MIME type');
  ensure(typeof result.data_base64 === 'string' && result.data_base64.length <= Math.ceil(SCREENSHOT_MAX_BYTES / 3) * 4, 'Screenshot exceeds limit');
  text(result.data_base64, /^[A-Za-z0-9+/]*={0,2}$/, 'Invalid screenshot base64');
  let decoded: string;
  try { decoded = atob(result.data_base64); } catch { throw new SessionAuthError('Invalid screenshot base64'); }
  ensure(decoded.length === result.byte_length && btoa(decoded) === result.data_base64, 'Screenshot size/base64 mismatch');
  if (result.mime_type === 'image/png') ensure(decoded.startsWith('\x89PNG\r\n\x1a\n'), 'Invalid PNG screenshot signature');
  else ensure(decoded.length >= 4 && decoded.startsWith('\xff\xd8') && decoded.endsWith('\xff\xd9'), 'Invalid JPEG screenshot boundaries');
  // macOS JPEG responses always carry this hash. Legacy QEMU PNG remains
  // compatible without it; a supplied hash is never silently ignored.
  if (result.mime_type === 'image/jpeg' || result.sha256 !== undefined) text(result.sha256, /^[0-9a-f]{64}$/, 'Invalid screenshot SHA-256');
  return Uint8Array.from(decoded, c => c.charCodeAt(0));
}
/** Snapshot before awaiting WebCrypto so mutation cannot replace the verified
 * payload with different bytes. A valid hash is integrity, not decoder success. */
export async function verifiedScreenshotSource(result: Record<string, unknown>): Promise<string> {
  const copy = { ...result };
  screenshotAnchor(copy);
  const bytes = screenshotBytes(copy);
  if (copy.sha256 !== undefined) ensure(await sha256(bytes) === copy.sha256, 'Screenshot SHA-256 mismatch');
  return `data:${copy.mime_type as string};base64,${copy.data_base64 as string}`;
}
