/** Physical-console control v1. Canonical bytes match Rust session-auth/control.rs.
 * Signatures authenticate requests; they do not encrypt HTTP, prove Guest input
 * delivery, or bind an RTP frame to a screenshot UUID. */
import { fileActionFields, validateFileResult, type FileAction } from './session-files.js';
import { DeviceIdentity, SessionAuthError, answerBytes, sha256, unhex, verifySignature, identityId, type AnswerTranscript } from './session-auth.js';

export interface FrameAnchor { frame_id: string; source_generation: number; width: number; height: number }
type Button = 'left' | 'middle' | 'right';
export type ControlAction =
  | { kind: 'file'; operation: FileAction }
  | { kind: 'observe_frame' }
  | { kind: 'click'; frame: FrameAnchor; x: number; y: number; button: Button }
  | { kind: 'drag'; frame: FrameAnchor; x: number; y: number; end_x: number; end_y: number; button: Button }
  | { kind: 'scroll'; frame: FrameAnchor; x: number; y: number; direction: 'up' | 'down' | 'left' | 'right'; ticks: number }
  | { kind: 'key'; frame: FrameAnchor; keys: string[] };
export interface ControlRequest { session_id: string; transcript_sha256: string; sequence: number; expires_at: number; action: ControlAction }
export interface SignedControl { request: ControlRequest; signature: string }
export interface ControlResponse { sequence: number; result: Record<string, unknown> }
export const CONTROL_MAX_RESPONSE_BYTES = 1024 * 1024;
export const SCREENSHOT_MAX_BYTES = 700 * 1024;
/** A control frame may be an integer box-downscale of the Guest display. `width`/`height` of the anchor stay the
 * SOURCE geometry (Guest pixels) and are what input coordinates are signed against; `frame_width`/`frame_height`
 * are the pixels of the image actually shown. Rule, shared with crates/media-local/src/control_frame.rs:
 *   frame = ceil(source / divisor); source -> frame: floor(s / d); frame -> source: min(f * d + floor(d / 2), source - 1).
 * frame -> source -> frame is exact; source -> frame -> source moves at most floor(d / 2) source pixels. */
export const CONTROL_FRAME_MAX_DIVISOR = 16;
export interface ControlFrameGeometry { source_width: number; source_height: number; frame_width: number; frame_height: number; scale_divisor: number }
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
  if (kind === 'observe_frame') { exact(action, ['kind']); c.text(kind); return; }
  const common = ['kind', 'frame'];
  if (kind === 'click') exact(action, [...common, 'x', 'y', 'button']);
  else if (kind === 'drag') exact(action, [...common, 'x', 'y', 'end_x', 'end_y', 'button']);
  else if (kind === 'scroll') exact(action, [...common, 'x', 'y', 'direction', 'ticks']);
  else if (kind === 'key') exact(action, [...common, 'keys']);
  else throw new SessionAuthError('Unsupported control action');
  c.text(kind); frameFields(action.frame, c);
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
 * itself establish a session, and never retries a request. */
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
/** Declared scale of a control frame, checked against the source geometry. A frame with no scale fields is
 * unscaled (the legacy PNG and the macOS JPEG); a PNG may never declare one. */
export function controlFrameGeometry(result: Record<string, unknown>): ControlFrameGeometry {
  const anchor = screenshotAnchor(result), declared = [result.frame_width, result.frame_height, result.scale_divisor];
  if (declared.every(value => value === undefined)) return { source_width: anchor.width, source_height: anchor.height, frame_width: anchor.width, frame_height: anchor.height, scale_divisor: 1 };
  ensure(result.mime_type !== 'image/png', 'Unscaled PNG screenshot must not declare a scale');
  integer(result.scale_divisor, 1, CONTROL_FRAME_MAX_DIVISOR); integer(result.frame_width, 1, 8192); integer(result.frame_height, 1, 8192);
  ensure(result.frame_width === Math.ceil(anchor.width / result.scale_divisor) && result.frame_height === Math.ceil(anchor.height / result.scale_divisor), 'Control frame scale does not match its source geometry');
  return { source_width: anchor.width, source_height: anchor.height, frame_width: result.frame_width, frame_height: result.frame_height, scale_divisor: result.scale_divisor };
}
/** The decoder's real image size must be the declared frame size before the image may anchor input. */
export function checkDecodedControlFrame(geometry: ControlFrameGeometry, naturalWidth: number, naturalHeight: number): void {
  ensure(naturalWidth === geometry.frame_width && naturalHeight === geometry.frame_height, 'Decoded screenshot dimensions do not match its input coordinates.');
}
export function sourceToFrame(source: number, divisor: number): number { integer(source); integer(divisor, 1, CONTROL_FRAME_MAX_DIVISOR); return Math.floor(source / divisor); }
export function frameToSource(frame: number, divisor: number, sourceExtent: number): number {
  integer(frame); integer(divisor, 1, CONTROL_FRAME_MAX_DIVISOR); integer(sourceExtent, 1, 8192);
  return Math.min(frame * divisor + Math.floor(divisor / 2), sourceExtent - 1);
}
/** Pointer offset inside the displayed frame element -> source pixel, through frame space: the element shows
 * `frameExtent` equal columns, each standing for `divisor` source pixels (the last block may be partial, hence the
 * clamp). So the result is always a source pixel of the block drawn under the pointer. Unscaled frames reduce to
 * floor(offset * source / displayed), the rule used before scaling existed. */
export function pointerToSource(offset: number, displayedExtent: number, frameExtent: number, divisor: number, sourceExtent: number): number {
  integer(sourceExtent, 1, 8192); integer(divisor, 1, CONTROL_FRAME_MAX_DIVISOR); integer(frameExtent, 1, 8192);
  ensure(frameExtent === Math.ceil(sourceExtent / divisor), 'Control frame scale does not match its source geometry');
  ensure(Number.isFinite(offset) && Number.isFinite(displayedExtent) && displayedExtent > 0 && offset >= 0 && offset < displayedExtent, 'Pointer outside the displayed screenshot');
  return Math.min(Math.floor(offset * frameExtent * divisor / displayedExtent), sourceExtent - 1);
}
/** Width and height from the first JPEG frame header; structural, not a decoder. */
function jpegDimensions(bytes: Uint8Array): { width: number; height: number } {
  let at = 2;
  for (;;) {
    ensure(at + 4 <= bytes.length && bytes[at] === 0xff, 'JPEG screenshot frame header missing');
    const marker = bytes[at + 1]!, length = bytes[at + 2]! * 256 + bytes[at + 3]!;
    ensure(length >= 2 && at + 2 + length <= bytes.length, 'JPEG screenshot segment truncated');
    if (marker >= 0xc0 && marker <= 0xcf && marker !== 0xc4 && marker !== 0xc8 && marker !== 0xcc) {
      ensure(length >= 7, 'JPEG screenshot frame header truncated');
      return { height: bytes[at + 5]! * 256 + bytes[at + 6]!, width: bytes[at + 7]! * 256 + bytes[at + 8]! };
    }
    ensure(marker !== 0xda && marker !== 0xd9, 'JPEG screenshot frame header missing');
    at += 2 + length;
  }
}
export function validateControlResponse(value: unknown, sequence: number, action: ControlAction): ControlResponse {
  exact(value, ['sequence', 'result']); integer(value.sequence, 1); ensure(value.sequence === sequence, 'Control response sequence substituted');
  ensure(value.result !== null && typeof value.result === 'object' && !Array.isArray(value.result), 'Invalid control result');
  const result = value.result as Record<string, unknown>;
  if (action.kind === 'file') { validateFileResult(result, action.operation); } else if (action.kind === 'observe_frame') {
    screenshotAnchor(result); screenshotBytes(result);
  } else { ensure(result.dispatched === true && result.guest_delivery_confirmed === false, 'Input dispatch not acknowledged'); }
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
  const bytes = Uint8Array.from(decoded, c => c.charCodeAt(0)), geometry = controlFrameGeometry(result);
  // A declared scale that disagrees with the image's own header never reaches the decoder.
  if (result.mime_type === 'image/jpeg') { const real = jpegDimensions(bytes); ensure(real.width === geometry.frame_width && real.height === geometry.frame_height, 'Control frame scale does not match its real dimensions'); }
  return bytes;
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

/** Direct interaction on the displayed control frame, within control v1. Pure state, no DOM, no network, and no
 * effect on canonical bytes or signing above. It lives in this module because the native service serves a fixed
 * allowlist of script files (crates/media-local/src/main.rs); a new module would need a server route.
 *
 * What control v1 offers: whole `click`, `drag`, `scroll` and `key` actions, each signed against a server-issued
 * screenshot anchor. It has no pointer move, no button down/up and no key down/up. So:
 *   - a press-move-release on the surface becomes ONE `drag`; hover and intermediate positions are never sent;
 *   - a key press becomes ONE `key` chord (modifiers held + the key), pressed and released by the host in one step.
 *     Nothing stays held in the Guest, so "release all" is purely local: forget modifiers, gestures and the queue;
 *   - keys are physical positions (KeyboardEvent.code -> QEMU qcode). There is no text, IME or clipboard action.
 */
export const INPUT_QUEUE_MAX = 8;
/** A queued input older than this when its turn comes is refused, visibly, instead of landing late. */
export const INPUT_MAX_AGE_MS = 6000;
export const SCROLL_MAX_TICKS = 20;
export const KEY_CHORD_MAX = 8;
/** Pointer travel in CSS pixels below which press-release is a click, not a drag. */
export const DRAG_THRESHOLD_CSS_PX = 5;

export type PointerButton = 'left' | 'middle' | 'right';
/** Source geometry a pointer coordinate was computed against; a later frame must match it exactly. */
export interface SourceShape { source_width: number; source_height: number; source_generation: number }
export type QueuedInput =
  | { kind: 'click'; at: number; shape: SourceShape; x: number; y: number; button: PointerButton; second: boolean }
  | { kind: 'drag'; at: number; shape: SourceShape; x: number; y: number; end_x: number; end_y: number; button: PointerButton }
  | { kind: 'scroll'; at: number; shape: SourceShape; x: number; y: number; direction: 'up' | 'down' | 'left' | 'right'; ticks: number }
  | { kind: 'key'; at: number; keys: string[]; repeat: boolean };

export function sameShape(a: SourceShape, b: SourceShape): boolean {
  return a.source_width === b.source_width && a.source_height === b.source_height && a.source_generation === b.source_generation;
}

/** Strict FIFO of inputs that are waiting; the one in flight has already been shifted out. Only two merges exist,
 * both into the tail so order is never changed: wheel ticks of the same direction at the same point, and keyboard
 * auto-repeat of the chord already waiting. Everything else that does not fit is reported as `full`; the caller
 * must refuse it visibly. Nothing is ever dropped from inside the queue. */
export class InputQueue {
  private readonly items: QueuedInput[] = [];
  constructor(readonly max: number = INPUT_QUEUE_MAX) {}
  get length(): number { return this.items.length; }
  get full(): boolean { return this.items.length >= this.max; }
  peek(): QueuedInput | undefined { return this.items[0]; }
  shift(): QueuedInput | undefined { return this.items.shift(); }
  /** Returns how many waiting inputs were removed. */
  clear(): number { return this.items.splice(0).length; }
  push(item: QueuedInput): 'queued' | 'merged' | 'capped' | 'full' {
    const tail = this.items.at(-1);
    if (tail && tail.kind === 'scroll' && item.kind === 'scroll' && tail.direction === item.direction && tail.x === item.x && tail.y === item.y && sameShape(tail.shape, item.shape)) {
      const total = tail.ticks + item.ticks;
      tail.ticks = Math.min(SCROLL_MAX_TICKS, total);
      return total > SCROLL_MAX_TICKS ? 'capped' : 'merged';
    }
    if (tail && tail.kind === 'key' && item.kind === 'key' && item.repeat && tail.keys.length === item.keys.length && tail.keys.every((key, index) => key === item.keys[index])) return 'merged';
    if (this.items.length >= this.max) return 'full';
    this.items.push(item); return 'queued';
  }
}

export interface Box { left: number; top: number; width: number; height: number }
/** Where a `frameWidth` x `frameHeight` picture is painted inside an element box under `object-fit: contain`:
 * scaled uniformly, centred, with bars on one axis when the aspect ratios differ. */
export function containedBox(box: Box, frameWidth: number, frameHeight: number): Box | null {
  if (!(box.width > 0) || !(box.height > 0) || !(frameWidth > 0) || !(frameHeight > 0)) return null;
  const scale = Math.min(box.width / frameWidth, box.height / frameHeight);
  const width = frameWidth * scale, height = frameHeight * scale;
  return { left: box.left + (box.width - width) / 2, top: box.top + (box.height - height) / 2, width, height };
}
function axis(offset: number, displayed: number, frame: number, divisor: number, source: number, clamp: boolean): number | null {
  if (!Number.isFinite(offset)) return null;
  if (offset < 0) return clamp ? 0 : null;
  if (offset >= displayed) return clamp ? source - 1 : null;
  return pointerToSource(offset, displayed, frame, divisor, source);
}
/** Client (CSS pixel) position -> Guest source pixel through the painted picture, using the existing
 * `pointerToSource` rule on each axis. `clientX`/`clientY` and the element box are both CSS pixels, so
 * devicePixelRatio and page zoom cancel; the canvas backing store is never consulted. A position on a letterbox
 * bar or outside the element is `null` (ignored) unless `clamp` is set, which is used only for the END of a drag
 * that left the picture: it is pinned to the nearest edge pixel. */
export function surfaceToSource(clientX: number, clientY: number, box: Box, geometry: ControlFrameGeometry, clamp = false): { x: number; y: number } | null {
  const painted = containedBox(box, geometry.frame_width, geometry.frame_height);
  if (!painted) return null;
  const x = axis(clientX - painted.left, painted.width, geometry.frame_width, geometry.scale_divisor, geometry.source_width, clamp);
  const y = axis(clientY - painted.top, painted.height, geometry.frame_height, geometry.scale_divisor, geometry.source_height, clamp);
  return x === null || y === null ? null : { x, y };
}

const WHEEL_PIXELS_PER_TICK = 100;
/** Wheel deltas -> whole discrete ticks on the dominant axis. Sub-tick trackpad deltas accumulate; a direction
 * change discards the remainder. */
export class WheelAccumulator {
  private x = 0; private y = 0;
  reset(): void { this.x = 0; this.y = 0; }
  add(deltaX: number, deltaY: number, deltaMode = 0): { direction: 'up' | 'down' | 'left' | 'right'; ticks: number } | null {
    const unit = deltaMode === 1 ? WHEEL_PIXELS_PER_TICK / 3 : deltaMode === 2 ? WHEEL_PIXELS_PER_TICK * 3 : 1;
    const dx = Number.isFinite(deltaX) ? deltaX * unit : 0, dy = Number.isFinite(deltaY) ? deltaY * unit : 0;
    if (dx === 0 && dy === 0) return null;
    if (Math.abs(dy) >= Math.abs(dx)) { this.x = 0; if (Math.sign(dy) !== Math.sign(this.y)) this.y = 0; this.y += dy; }
    else { this.y = 0; if (Math.sign(dx) !== Math.sign(this.x)) this.x = 0; this.x += dx; }
    const vertical = this.y !== 0, value = vertical ? this.y : this.x;
    const ticks = Math.min(SCROLL_MAX_TICKS, Math.floor(Math.abs(value) / WHEEL_PIXELS_PER_TICK));
    if (ticks < 1) return null;
    const direction = vertical ? (value > 0 ? 'down' : 'up') : (value > 0 ? 'right' : 'left');
    this.reset();
    return { direction, ticks };
  }
}

const MODIFIERS: readonly { code: string; qcode: string; flag: 'ctrlKey' | 'altKey' | 'shiftKey' | 'metaKey' }[] = [
  { code: 'ControlLeft', qcode: 'ctrl', flag: 'ctrlKey' }, { code: 'ControlRight', qcode: 'ctrl_r', flag: 'ctrlKey' },
  { code: 'AltLeft', qcode: 'alt', flag: 'altKey' }, { code: 'AltRight', qcode: 'alt_r', flag: 'altKey' },
  { code: 'ShiftLeft', qcode: 'shift', flag: 'shiftKey' }, { code: 'ShiftRight', qcode: 'shift_r', flag: 'shiftKey' },
  { code: 'MetaLeft', qcode: 'meta_l', flag: 'metaKey' }, { code: 'MetaRight', qcode: 'meta_r', flag: 'metaKey' },
];
/** KeyboardEvent.code -> qcode, limited to the controller's allowlist (crates/control/src/input.rs key_names).
 * Positions only: the Guest's own layout decides which character a position produces. */
const KEYS: Readonly<Record<string, string>> = (() => {
  const keys: Record<string, string> = {
    Enter: 'ret', Escape: 'esc', Tab: 'tab', Space: 'spc', Backspace: 'backspace', Delete: 'delete', Insert: 'insert',
    Home: 'home', End: 'end', PageUp: 'pgup', PageDown: 'pgdn', ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right',
    Minus: 'minus', Equal: 'equal', BracketLeft: 'bracket_left', BracketRight: 'bracket_right', Semicolon: 'semicolon',
    Quote: 'apostrophe', Backquote: 'grave_accent', Backslash: 'backslash', Comma: 'comma', Period: 'dot', Slash: 'slash',
  };
  for (let i = 0; i < 26; i++) keys[`Key${String.fromCharCode(65 + i)}`] = String.fromCharCode(97 + i);
  for (let i = 0; i < 10; i++) keys[`Digit${i}`] = String(i);
  for (let i = 1; i <= 12; i++) keys[`F${i}`] = `f${i}`;
  return keys;
})();
export interface ModifierFlags { ctrlKey?: boolean; altKey?: boolean; shiftKey?: boolean; metaKey?: boolean }
export function anyModifier(flags: ModifierFlags): boolean { return flags.ctrlKey === true || flags.altKey === true || flags.shiftKey === true || flags.metaKey === true; }
export function isModifierCode(code: string): boolean { return MODIFIERS.some(modifier => modifier.code === code); }

/** Local view of which modifier keys are down. It only shapes the next chord; the Guest never holds a key. */
export class ModifierTracker {
  private readonly down = new Set<string>();
  private lone: string | null = null;
  /** Returns true when `code` is a modifier (the caller sends nothing for the press itself). */
  press(code: string, repeat = false): boolean {
    if (!isModifierCode(code)) { this.lone = null; return false; }
    if (!repeat) { this.down.add(code); this.lone = code; }
    return true;
  }
  /** A modifier released with no other key or pointer use since its press is a tap of that key: returns the chord
   * to send, which is any modifiers still held plus the tapped one (so Alt+Shift reaches the Guest as a chord). */
  release(code: string): string[] | null {
    if (!this.down.delete(code)) return null;
    const tapped = this.lone === code; this.lone = null;
    return tapped ? [...this.held(), MODIFIERS.find(modifier => modifier.code === code)!.qcode] : null;
  }
  /** Pointer use or any refusal between press and release means the release is not a tap. */
  interrupt(): void { this.lone = null; }
  clear(): void { this.down.clear(); this.lone = null; }
  /** Modifiers for a chord, reconciled with the event's own flags: a key whose release was missed (focus was
   * elsewhere) is dropped; a flag with no tracked key uses the left key. */
  held(flags: ModifierFlags = {}): string[] {
    const result: string[] = [];
    for (const flag of ['ctrlKey', 'altKey', 'shiftKey', 'metaKey'] as const) {
      const family = MODIFIERS.filter(modifier => modifier.flag === flag);
      if (flags[flag] === false) { for (const modifier of family) this.down.delete(modifier.code); continue; }
      const tracked = family.filter(modifier => this.down.has(modifier.code)).map(modifier => modifier.qcode);
      if (tracked.length > 0) result.push(...tracked); else if (flags[flag] === true) result.push(family[0]!.qcode);
    }
    return result;
  }
}
/** One whole chord for a non-modifier key, or null when the position has no qcode in the controller allowlist. */
export function keyChord(code: string, held: readonly string[]): string[] | null {
  const qcode = KEYS[code];
  if (!qcode) return null;
  const chord = [...held.filter((key, index) => held.indexOf(key) === index), qcode];
  return chord.length > KEY_CHORD_MAX ? null : chord;
}

export type LinkState = 'idle' | 'connecting' | 'live' | 'reconnecting' | 'ended';
export type SessionState = 'idle' | 'connecting' | 'live' | 'input-pending' | 'frame-stale' | 'reconnecting' | 'ended';
/** One visible state. Transport states win; then what the user's input is doing; then whether the picture input
 * would be bound to is still fresh. `frame-stale` is reported only while the session may send input. */
export function sessionState(input: { link: LinkState; reason: string; sending: boolean; waiting: number; queueMax: number; inputAllowed: boolean; frameFresh: boolean; graceMs: number }): { state: SessionState; text: string } {
  if (input.link === 'ended') return { state: 'ended', text: `Ended: ${input.reason || 'the session was closed.'}` };
  if (input.link === 'idle') return { state: 'idle', text: 'Not connected.' };
  if (input.link === 'reconnecting') return { state: 'reconnecting', text: `Reconnecting: the connection dropped; waiting up to ${Math.round(input.graceMs / 1000)} seconds. Input is refused until it returns.` };
  if (input.sending || input.waiting > 0) {
    const full = input.waiting >= input.queueMax;
    return { state: 'input-pending', text: `Input pending: ${input.sending ? '1 sending' : 'none sending'}, ${input.waiting} waiting${full ? ` (queue full at ${input.queueMax}; further input is refused)` : ''}.` };
  }
  if (input.inputAllowed && !input.frameFresh) return { state: 'frame-stale', text: 'Picture stale: the control picture is older than 4 seconds. Input is refused until it is refreshed.' };
  if (input.link === 'connecting') return { state: 'connecting', text: 'Connecting…' };
  return { state: 'live', text: input.inputAllowed ? 'Live. Input enabled.' : 'Live. Viewing only.' };
}
