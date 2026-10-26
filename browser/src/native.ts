import { SessionFiles, FileOperationError, FILE_MAX_BYTES } from './session-files.js';
import { icePeerConfiguration, iceConfigurationSummary, selectedIcePath, iceCandidateCounts, iceStatsCounts, videoReceptionCounts } from './ice-config.js';
import { AuthenticatedReceiver } from './native-auth.js';
import { videoFeedbackSummary } from './feedback-evidence.js';
import { EncryptedControlChannel, ControlFrameUnavailableError } from './control-channel.js';
import type { SignedGrant, Scope } from './session-auth.js';
import { screenshotAnchor, verifiedScreenshotSource, controlFrameGeometry, checkDecodedControlFrame, type FrameAnchor, type ControlFrameGeometry, type ControlAction, type ControlResponse,
  InputQueue, ModifierTracker, WheelAccumulator, surfaceToSource, sameShape, keyChord, anyModifier, sessionState,
  INPUT_QUEUE_MAX, INPUT_MAX_AGE_MS, DRAG_THRESHOLD_CSS_PX, type QueuedInput, type SourceShape, type PointerButton, type LinkState } from './session-control.js';
import { codecName, nativeCodecPreferences, createNativeCodecPlan, negotiateCodec, ReceptionDeadline, DisconnectGrace, DISCONNECT_GRACE_MS, mediaFailureFromReceipt, SourceFailureError, type MediaFailure, CodecError, NegotiationError, type HostCapabilities, type AttemptResult, type VideoCodec } from './codec-policy.js';

const video = document.querySelector<HTMLVideoElement>('#received')!;
const lastFrame = document.querySelector<HTMLCanvasElement>('#last-frame')!;
const output = document.querySelector<HTMLPreElement>('#result')!;
const connect = document.querySelector<HTMLButtonElement>('#connect')!;
const disconnect = document.querySelector<HTMLButtonElement>('#disconnect')!;
const retryCleanup = document.querySelector<HTMLButtonElement>('#retry-cleanup')!;
const status = document.querySelector<HTMLElement>('#status')!;
let host: HostCapabilities | null = null;
let authenticationRequired = false;
let iceConfigurationEnabled = false;
let authVmId = "";
let authenticated: AuthenticatedReceiver | null = null;
let grantReady = false;
let controller: AbortController | null = null;
let closeCurrent: (() => Promise<void>) | null = null;
let cleanup: Promise<void> = Promise.resolve();
let negotiationSourceFailure: MediaFailure | null = null;
let cleanupState: 'idle' | 'pending' | 'failed' = 'idle';
let retryCurrentCleanup: (() => Promise<void>) | null = null;
function updateConnectionControls() {
  connect.disabled = !!controller || !host || cleanupState !== 'idle' || (authenticationRequired && !grantReady);
  retryCleanup.disabled = cleanupState !== 'failed';
  if (cleanupState === 'failed') retryCleanup.hidden = false;
  else if (cleanupState === 'idle') retryCleanup.hidden = true;
}
let report: Record<string, unknown> = {};
let activeRequest: { request_id: string; session_id?: string } | null = null;
let reportBatch = 0, reportDirty = false;
function renderReport() { reportDirty = false; output.textContent = JSON.stringify(report, null, 2); }
function show(values: Record<string, unknown>) { report = { ...report, ...values }; if (reportBatch > 0) reportDirty = true; else renderReport(); }
// One synchronous stats tick publishes several report sections; serialise them into the DOM once.
function batchReport(update: () => void) { reportBatch++; try { update(); } finally { if (--reportBatch === 0 && reportDirty) renderReport(); } }
const controlPanel = document.querySelector<HTMLFieldSetElement>('#desktop-control')!;
const controlStatus = document.querySelector<HTMLElement>('#control-status')!;
const controlFrame = document.querySelector<HTMLCanvasElement>('#control-frame')!;
const refreshControlFrame = document.querySelector<HTMLButtonElement>('#refresh-control-frame')!;
const sendPhysicalKey = document.querySelector<HTMLButtonElement>('#send-physical-key')!;
const requestFiles = document.querySelector<HTMLInputElement>('#request-files')!;
const filePanel = document.querySelector<HTMLFieldSetElement>('#file-transfer')!;
const fileStatus = document.querySelector<HTMLElement>('#file-status')!;
const uploadFile = document.querySelector<HTMLInputElement>('#upload-file')!;
const uploadButton = document.querySelector<HTMLButtonElement>('#upload-file-button')!;
const downloadButton = document.querySelector<HTMLButtonElement>('#download-file-button')!;
const cancelFile = document.querySelector<HTMLButtonElement>('#cancel-file')!;
let filesEnabled = false, sessionFiles: SessionFiles | null = null;
let pendingFile: Promise<unknown> | null = null;
const requestInput = document.querySelector<HTMLInputElement>('#request-input')!;
const resetPairing = document.querySelector<HTMLButtonElement>('#reset-pairing')!;
let controlEnabled = false, controlSessionReady = false;
let requestedScopes: Scope[] = ['observe'];
let controlGeneration = 0;
let controlReceiver: AuthenticatedReceiver | null = null;
let pendingControl: Promise<ControlResponse> | null = null;
let observedFrame: { anchor: FrameAnchor; geometry: ControlFrameGeometry; expires: number } | null = null;
let dragStart: { x: number; y: number } | null = null;
let frameExpiry: ReturnType<typeof setTimeout> | undefined;
// Direct interaction (see the direct-input section of session-control.ts). `displayed` is the picture currently
// painted on the control surface; unlike `observedFrame` it survives being consumed by an input, so a pointer
// position can still be read against what the user is looking at while its turn in the queue is pending.
const directControl = document.querySelector<HTMLInputElement>('#direct-control')!;
const stepwiseControls = document.querySelector<HTMLElement>('#stepwise-controls')!;
const sessionStateView = document.querySelector<HTMLElement>('#session-state')!;
const inputNotice = document.querySelector<HTMLElement>('#input-notice')!;
const FRAME_FRESH_MS = 4000;
let displayed: { geometry: ControlFrameGeometry; shape: SourceShape; expires: number } | null = null;
const directQueue = new InputQueue(INPUT_QUEUE_MAX);
const modifiers = new ModifierTracker(), wheel = new WheelAccumulator();
const queueCounters = { queued: 0, merged: 0, sent: 0, refused: 0, discarded: 0 };
let pumping = false, sendingDirect = false, surfaceFocused = false;
// Control picture diagnostics. `shown` counts pictures that became usable; `expired` counts pictures that arrived
// only after the freshness bound and were therefore not shown; `joined` counts refresh requests that were answered
// by the fetch already in progress instead of a second request; `unavailable` counts requests the host answered
// with the typed "frame unavailable" reply (no picture, nothing fenced), with the host's fixed reason word.
const frameCounters = { shown: 0, expired: 0, consecutiveExpired: 0, joined: 0, lastExpiredMs: null as number | null, unavailable: 0, consecutiveUnavailable: 0, lastUnavailableReason: null as string | null };
// The one picture fetch in progress, from request to decoded picture. A fetch superseded by a control change
// (its generation is no longer current) abandons itself and is not joined.
let frameRefresh: { generation: number; done: Promise<void> } | null = null;
// After a picture that arrived too late or that the host could not produce, at most this many are fetched again
// without a user action. Both kinds share the one budget, so alternating them cannot become a loop.
const SLOW_FRAME_AUTO_RETRIES = 1;
function frameRetryAllowed() { return keepFresh() && frameCounters.consecutiveExpired + frameCounters.consecutiveUnavailable <= SLOW_FRAME_AUTO_RETRIES; }
/** The host's fixed reason word in plain words. An unknown word (already checked to be a-z and _) is shown as is. */
const UNAVAILABLE_WORDS: Record<string, string> = {
  controller_refused: 'the host had no picture ready',
  controller_deadline: 'the host did not answer in time',
  png_decode_unsupported: 'the host could not convert the picture',
  scaled_frame_over_cap: 'the picture is too large to send',
};
function unavailableWords(reason: string) { return Object.hasOwn(UNAVAILABLE_WORDS, reason) ? UNAVAILABLE_WORDS[reason]! : `host reason: ${reason.replaceAll('_', ' ')}`; }
// The refusal currently on the notice line, so an identical one can be counted there instead of looking unchanged.
let refusalNotice: { message: string; shown: string; count: number } | null = null;
function publishFrameCounters() { show({ controlFrameRefresh: { ...frameCounters, freshForMs: FRAME_FRESH_MS } }); }
function seconds(ms: number) { return (ms / 1000).toFixed(1); }
/** Which picture the host sent: geometry, scale and encoding. Never the picture bytes. */
function describeFrame(geometry: ControlFrameGeometry, result: Record<string, unknown>, loadMs: number, usable: boolean) {
  const jpeg = result.mime_type === 'image/jpeg', scaled = geometry.scale_divisor > 1;
  const path = scaled ? 'scaled-jpeg' : jpeg ? 'full-jpeg' : 'full-png';
  const text = `Picture: ${geometry.frame_width}x${geometry.frame_height} ${jpeg ? 'JPEG' : 'PNG'}, ${scaled ? `scaled 1/${geometry.scale_divisor} from the ${geometry.source_width}x${geometry.source_height} display` : 'full size'}; fetched in ${seconds(loadMs)} s.`;
  return { text, report: { ...geometry, path, mimeType: result.mime_type, byteLength: result.byte_length, requestToReadyMs: Math.round(loadMs), usable } };
}
let gesture: { button: PointerButton; x: number; y: number; clientX: number; clientY: number; shape: SourceShape } | null = null;
let lastClick: { at: number; button: PointerButton; clientX: number; clientY: number } | null = null;
let linkState: LinkState = 'idle', linkReason = '';
function directReady() { return directControl.checked && controlEnabled && controlSessionReady && cleanupState === 'idle' && !!controlReceiver && requestedScopes.includes('input'); }
/** The session was granted input. There is no control lease: each input is bound to a fresh picture instead. */
function inputAllowed() { return controlEnabled && controlSessionReady && requestedScopes.includes('input'); }
function displayedFresh() { return !!displayed && performance.now() < displayed.expires; }
function renderSessionState() {
  const view = sessionState({ link: linkState, reason: linkReason, sending: sendingDirect, waiting: directQueue.length, queueMax: directQueue.max, inputAllowed: inputAllowed(), frameFresh: displayedFresh(), graceMs: DISCONNECT_GRACE_MS });
  if (sessionStateView.textContent === view.text) return;
  sessionStateView.textContent = view.text; sessionStateView.setAttribute('data-state', view.state);
}
function publishQueue() { show({ controlQueue: { ...queueCounters, waiting: directQueue.length, max: directQueue.max } }); }
/** Forget everything local that a later input could inherit: the gesture in progress, tracked modifiers, the
 * wheel remainder and every input still waiting. The Guest holds no key or button between actions, so there is
 * nothing to release remotely; the one action already in flight cannot be recalled and is never retried. */
function releaseAll(reason: string): number {
  gesture = null; lastClick = null; modifiers.clear(); wheel.reset();
  const dropped = directQueue.clear();
  if (dropped > 0) {
    queueCounters.discarded += dropped;
    inputNotice.textContent = `${dropped} waiting input${dropped === 1 ? ' was' : 's were'} discarded and NOT sent: ${reason}.`;
    publishQueue();
  }
  renderSessionState();
  return dropped;
}
function setLink(state: LinkState, reason = '') {
  linkState = state; linkReason = reason;
  if (state === 'reconnecting') releaseAll('the connection dropped');
  renderSessionState();
}
function updateControlControls() {
  const available = controlEnabled && controlSessionReady && cleanupState === 'idle' && !pendingControl && !pendingFile;
  const filesAvailable = filesEnabled && requestedScopes.includes('files') && controlSessionReady && cleanupState === 'idle' && !!sessionFiles;
  filePanel.hidden = !filesEnabled;
  uploadButton.disabled = downloadButton.disabled = !filesAvailable || !!pendingFile || !!pendingControl;
  cancelFile.disabled = !filesAvailable || !pendingFile;
  const canInput = available && requestedScopes.includes('input');
  const fresh = !!observedFrame && performance.now() < observedFrame.expires;
  const direct = directControl.checked;
  controlPanel.hidden = !controlEnabled; stepwiseControls.hidden = direct;
  refreshControlFrame.disabled = !available;
  sendPhysicalKey.disabled = direct || !canInput || !fresh;
  // In direct mode the picture stays the input surface between an input and its automatic refresh.
  const usable = direct ? directReady() && displayedFresh() : canInput && fresh;
  controlFrame.setAttribute('aria-disabled', String(!usable));
  controlFrame.style.opacity = (direct ? displayedFresh() : fresh) ? '1' : '0.5';
  renderSessionState();
}
function clearControlFrame(message: string) {
  controlGeneration++; observedFrame = null; dragStart = null; displayed = null;
  clearTimeout(frameExpiry);
  releaseAll('control ended');
  controlStatus.textContent = message; updateControlControls();
}
function endControlSession() {
  sessionFiles?.dispose(); sessionFiles = null; uploadFile.value = '';
  fileStatus.textContent = 'Disconnected. File cleanup requires the session close acknowledgment.';
  controlSessionReady = false; controlReceiver = null; frameCounters.consecutiveExpired = 0; frameCounters.consecutiveUnavailable = 0;
  clearControlFrame('Control is disconnected. Connect to send input.');
}
function controlReady() { return controlEnabled && controlSessionReady && cleanupState === 'idle' && !!controlReceiver && !pendingControl && !pendingFile; }
async function sendControl(action: ControlAction): Promise<Record<string, unknown>> {
  if (!controlReady()) throw new Error('Control is unavailable or another operation is pending.');
  const receiver = controlReceiver!;
  const requestedAt = performance.now();
  show({ controlAction: action.kind, controlRequestedAt: new Date().toISOString(), controlDispatched: false, controlError: null });
  const operation = receiver.control(action, AbortSignal.timeout(25000));
  pendingControl = operation; updateControlControls();
  try { const result = (await operation).result; show({ controlCompletedAt: new Date().toISOString(), controlRoundTripMs: performance.now() - requestedAt }); return result; }
  catch (error) {
    // "No picture this time" fences nothing: the session and the channel are kept. fetchFrame reports it.
    if (error instanceof ControlFrameUnavailableError && action.kind === 'observe_frame') {
      show({ controlCompletedAt: new Date().toISOString(), controlRoundTripMs: performance.now() - requestedAt }); throw error;
    }
    clearControlFrame('Control request failed. Refresh the screenshot before further input.');
    show({ controlError: String(error), controlAction: action.kind, controlCompletedAt: new Date().toISOString(), controlRoundTripMs: performance.now() - requestedAt }); throw error;
  } finally { if (pendingControl === operation) pendingControl = null; updateControlControls(); }
}
function inputState(): { frame: FrameAnchor } | null {
  if (!controlReady() || !requestedScopes.includes('input')) return null;
  if (!observedFrame || performance.now() >= observedFrame.expires) {
    observedFrame = null; dragStart = null; controlStatus.textContent = 'Screenshot expired. Refresh it before sending input.'; updateControlControls(); return null;
  }
  return { frame: { ...observedFrame.anchor } };
}
async function dispatchInput(action: ControlAction) {
  const generation = controlGeneration;
  try {
    await sendControl(action);
    if (generation !== controlGeneration) return;
    observedFrame = null; dragStart = null; clearTimeout(frameExpiry);
    controlStatus.textContent = 'Input dispatched. Refreshing the screenshot to check the result; application delivery is not confirmed.';
    show({ controlAction: action.kind, controlDispatched: true, guestDeliveryConfirmed: false });
    // sendControl has already cleared its pending request, so this refresh is
    // admitted. The next input still needs the fresh, bound frame it returns.
    void refreshFrame();
  } catch { /* sendControl already clears the frame and records a bounded error. */ }
  finally { updateControlControls(); }
}
/** Fetch one control picture. There is never more than one fetch in progress: a call made while one is running
 * (request, verification or decoding) is answered by that fetch. */
function refreshFrame(): Promise<void> {
  if (frameRefresh && frameRefresh.generation === controlGeneration) { frameCounters.joined++; return frameRefresh.done; }
  if (!controlReady()) return Promise.resolve();
  const entry = { generation: ++controlGeneration, done: Promise.resolve() };
  frameRefresh = entry;
  // The retry runs after the fetch has ended and released `frameRefresh`, so it is a new, single fetch.
  entry.done = fetchFrame(entry).then(retry => (retry && keepFresh() ? refreshFrame() : undefined));
  return entry.done;
}
/** One fetch. Resolves true when the picture arrived too late, or the host had none, and one more may be fetched
 * without a user action. */
async function fetchFrame(entry: { generation: number }): Promise<boolean> {
  const generation = entry.generation, started = performance.now();
  let retry = false;
  observedFrame = null; dragStart = null; clearTimeout(frameExpiry); updateControlControls();
  try {
    const result = await sendControl({ kind: 'observe_frame' });
    // The anchor is the source (Guest pixel) geometry; the image itself may be a declared integer downscale.
    const anchor = screenshotAnchor(result), geometry = controlFrameGeometry(result);
    const image = new Image(); image.src = await verifiedScreenshotSource(result);
    await image.decode();
    if (generation !== controlGeneration || !controlSessionReady) return false;
    checkDecodedControlFrame(geometry, image.naturalWidth, image.naturalHeight);
    const loadMs = performance.now() - started;
    if (performance.now() >= started + FRAME_FRESH_MS) {
      // The picture is verified and decoded but already older than an input may be bound to. The freshness bound
      // is unchanged: it is not shown and anchors nothing, so only input is paused until a picture arrives in time.
      frameCounters.expired++; frameCounters.consecutiveExpired++; frameCounters.lastExpiredMs = Math.round(loadMs);
      retry = frameRetryAllowed();
      const described = describeFrame(geometry, result, loadMs, false);
      batchReport(() => { show({ controlFrame: described.report }); publishFrameCounters(); });
      controlStatus.textContent = `The control picture took ${seconds(loadMs)} s to refresh, longer than the ${FRAME_FRESH_MS / 1000} seconds a picture may be used for, so it was not shown. `
        + (inputAllowed() ? 'Input is paused. ' : '')
        + (retry ? 'Fetching it again…' : 'Press Refresh screenshot to try again.');
      return retry;
    }
    const context = controlFrame.getContext('2d'); if (!context) throw new Error('Screenshot canvas unavailable.');
    // Canvas holds exactly the frame's own pixels at the frame's own aspect; pointer offsets go through frame
    // space to source pixels (pointerToSource), so a click lands in the source block drawn under it.
    controlFrame.width = geometry.frame_width; controlFrame.height = geometry.frame_height; controlFrame.style.aspectRatio = `${geometry.frame_width} / ${geometry.frame_height}`;
    context.drawImage(image, 0, 0, geometry.frame_width, geometry.frame_height); controlFrame.hidden = false;
    observedFrame = { anchor, geometry, expires: started + FRAME_FRESH_MS };
    displayed = { geometry, shape: { source_width: geometry.source_width, source_height: geometry.source_height, source_generation: anchor.source_generation }, expires: observedFrame.expires };
    // A gesture begun on a picture of another geometry cannot be finished on this one.
    if (gesture && !sameShape(gesture.shape, displayed.shape)) gesture = null;
    frameExpiry = setTimeout(() => {
      observedFrame = null; dragStart = null; controlStatus.textContent = 'Screenshot expired. Refresh it before sending input.'; updateControlControls();
      // While the user is on the surface with input enabled, keep the picture usable: one bounded refresh per expiry.
      if (keepFresh()) void refreshFrame();
    }, Math.max(0, observedFrame.expires - performance.now()));
    frameCounters.shown++; frameCounters.consecutiveExpired = 0; frameCounters.consecutiveUnavailable = 0;
    const described = describeFrame(geometry, result, loadMs, true);
    batchReport(() => { show({ controlFrame: described.report }); publishFrameCounters(); });
    controlStatus.textContent = (!requestedScopes.includes('input') ? 'Screenshot ready for observation.'
      : directControl.checked ? 'Screenshot ready. Click, drag, scroll or type on the picture; each input is sent in order and followed by a fresh picture.'
      : 'Screenshot ready for 4 seconds. Choose an action and click the screenshot.') + ' ' + described.text;
  } catch (error) {
    if (generation !== controlGeneration) return false;
    if (error instanceof ControlFrameUnavailableError) {
      // The host answered, in sequence, that it has no usable picture. Nothing is shown and nothing is anchored, so
      // input stays paused by the unchanged freshness rule; the session is untouched.
      frameCounters.unavailable++; frameCounters.consecutiveUnavailable++; frameCounters.lastUnavailableReason = error.reason;
      retry = frameRetryAllowed();
      publishFrameCounters();
      controlStatus.textContent = `No control picture this time (${unavailableWords(error.reason)}). ` + (retry ? 'Input is paused; fetching it again…' : 'Input is paused; press Refresh screenshot.');
      return retry;
    }
    clearControlFrame('Screenshot unavailable. Refresh it before input.'); show({ controlError: String(error), controlAction: 'observe_frame' }); }
  finally { if (frameRefresh === entry) frameRefresh = null; updateControlControls(); }
  return false;
}
refreshControlFrame.addEventListener('click', () => { void refreshFrame(); });
const physicalKeys = new Set(['ret', 'esc', 'tab', 'backspace', 'delete', 'up', 'down', 'left', 'right', 'home', 'end', 'pgup', 'pgdn', 'spc', 'ctrl,a']);
sendPhysicalKey.addEventListener('click', () => {
  if (directControl.checked) return;
  const state = inputState(), selection = document.querySelector<HTMLSelectElement>('#physical-key')!.value;
  if (!state || !physicalKeys.has(selection)) return;
  void dispatchInput({ kind: 'key', ...state, keys: selection.split(',') });
});
document.querySelector<HTMLSelectElement>('#pointer-action')!.addEventListener('change', () => { dragStart = null; });
controlFrame.addEventListener('click', event => {
  // Direct mode takes pointer events itself; the click that follows a pointer release must not send a second input.
  if (directControl.checked) return;
  const state = inputState(); if (!state) return;
  const bounds = controlFrame.getBoundingClientRect();
  if (bounds.width <= 0 || bounds.height <= 0 || event.clientX < bounds.left || event.clientY < bounds.top || event.clientX >= bounds.right || event.clientY >= bounds.bottom) return;
  const shown = observedFrame?.geometry; if (!shown || shown.source_width !== state.frame.width || shown.source_height !== state.frame.height) return;
  // The picture is painted with object-fit: contain, so a position on a letterbox bar is not a Guest pixel.
  const point = surfaceToSource(event.clientX, event.clientY, bounds, shown); if (!point) return;
  const { x, y } = point;
  const mode = document.querySelector<HTMLSelectElement>('#pointer-action')!.value;
  if (mode === 'drag') {
    if (!dragStart) { dragStart = { x, y }; controlStatus.textContent = 'Drag start selected. Click the destination on the same fresh screenshot.'; return; }
    const start = dragStart; dragStart = null;
    void dispatchInput({ kind: 'drag', ...state, x: start.x, y: start.y, end_x: x, end_y: y, button: 'left' });
  } else if (mode === 'scroll-up' || mode === 'scroll-down') void dispatchInput({ kind: 'scroll', ...state, x, y, direction: mode === 'scroll-up' ? 'up' : 'down', ticks: 3 });
  else if (mode === 'click' || mode === 'right-click') void dispatchInput({ kind: 'click', ...state, x, y, button: mode === 'click' ? 'left' : 'right' });
});

// ---- Direct interaction on the control picture -------------------------------------------------------------
// Pointer, wheel and keyboard events on the surface become the existing signed actions. Strictly one in flight,
// in arrival order; each input is bound to the frame that is current when its turn comes and is followed by one
// fresh frame, exactly as the step-by-step path does. The only exception is the second click of a double-click,
// which reuses the anchor of the first (the same picture the user double-clicked on) so no screenshot round trip
// sits between the two clicks.
function keepFresh() { return directReady() && surfaceFocused && linkState !== 'reconnecting' && document.visibilityState !== 'hidden' && controlReady(); }
function refuse(message: string) {
  queueCounters.refused++; modifiers.interrupt();
  // Every refusal is counted in the report at once. An identical one in a row is shown as a count on the same
  // notice line (one bounded string, no log), so a burst (wheel, auto-repeat) is visible without growing anything.
  const repeated = !!refusalNotice && refusalNotice.message === message && inputNotice.textContent === refusalNotice.shown;
  const count = repeated ? refusalNotice!.count + 1 : 1;
  const shown = count === 1 ? message : `${message} (${count} inputs in a row were refused for this reason.)`;
  refusalNotice = { message, shown, count };
  inputNotice.textContent = shown; publishQueue(); renderSessionState();
}
/** Why an input may not even wait in the queue right now, or null. A stale picture also starts one refresh. */
function refusal(): string | null {
  if (!directReady()) return 'Input is unavailable: no connected control session with input permission. Nothing was sent.';
  if (linkState === 'reconnecting') return 'The connection is recovering. Nothing was sent; try again when it is live.';
  if (pendingFile) return 'A file transfer is in progress. Nothing was sent.';
  if (!displayedFresh()) {
    if (controlReady()) void refreshFrame();
    return 'The picture was older than 4 seconds, so this input was NOT sent. A fresh picture is being fetched; repeat the input on it.';
  }
  return null;
}
function enqueue(item: QueuedInput, label: string) {
  const reason = refusal(); if (reason) { refuse(reason); return; }
  const outcome = directQueue.push(item);
  if (outcome === 'full') { refuse(`The input queue is full (${directQueue.max} waiting). This ${label} was NOT sent; wait for the queue to drain.`); return; }
  if (outcome === 'queued') queueCounters.queued++; else queueCounters.merged++;
  inputNotice.textContent = outcome === 'capped' ? 'Scroll is limited to 20 ticks per action; the extra ticks were not sent.' : '';
  publishQueue(); renderSessionState();
  void pumpDirect();
}
function toAction(item: QueuedInput, frame: FrameAnchor): ControlAction {
  if (item.kind === 'key') return { kind: 'key', frame, keys: [...item.keys] };
  if (item.kind === 'click') return { kind: 'click', frame, x: item.x, y: item.y, button: item.button };
  if (item.kind === 'drag') return { kind: 'drag', frame, x: item.x, y: item.y, end_x: item.end_x, end_y: item.end_y, button: item.button };
  return { kind: 'scroll', frame, x: item.x, y: item.y, direction: item.direction, ticks: item.ticks };
}
async function pumpDirect() {
  if (pumping) return;
  pumping = true;
  try {
    for (;;) {
      // A manual refresh may be in flight; the host admits one request at a time and queues none.
      if (pendingControl) { await pendingControl.catch(() => {}); continue; }
      const next = directQueue.peek(); if (!next) break;
      const now = performance.now();
      if (!directReady() || pendingFile) { releaseAll('control is no longer available'); break; }
      if (now - next.at > INPUT_MAX_AGE_MS) { releaseAll(`they had waited more than ${INPUT_MAX_AGE_MS / 1000} seconds`); break; }
      if (!observedFrame || now >= observedFrame.expires) {
        await refreshFrame();
        if (!observedFrame && directQueue.length > 0) { releaseAll('no fresh picture could be obtained'); break; }
        continue;
      }
      if (next.kind !== 'key' && (!displayed || !sameShape(next.shape, displayed.shape))) { releaseAll('the Guest display changed size or source'); break; }
      directQueue.shift();
      const generation = controlGeneration;
      sendingDirect = true; renderSessionState();
      let sent = false;
      try { await sendControl(toAction(next, { ...observedFrame.anchor })); sent = generation === controlGeneration; }
      catch { /* sendControl already cleared the frame, recorded the error and discarded the queue */ }
      finally { sendingDirect = false; }
      if (!sent) break;
      queueCounters.sent++;
      batchReport(() => { show({ controlAction: next.kind, controlDispatched: true, guestDeliveryConfirmed: false }); publishQueue(); });
      const follower = directQueue.peek();
      if (next.kind === 'click' && follower?.kind === 'click' && follower.second && follower.button === next.button && observedFrame && performance.now() < observedFrame.expires) continue;
      observedFrame = null; clearTimeout(frameExpiry);
      controlStatus.textContent = 'Input dispatched. Refreshing the picture to show the result; application delivery is not confirmed.';
      await refreshFrame();
    }
  } finally { pumping = false; sendingDirect = false; updateControlControls(); }
}
type SurfaceEvent = { preventDefault?: () => void };
function consume(event: SurfaceEvent) { event.preventDefault?.(); }
function surfacePoint(event: { clientX: number; clientY: number }, clamp: boolean) {
  return displayed ? surfaceToSource(event.clientX, event.clientY, controlFrame.getBoundingClientRect(), displayed.geometry, clamp) : null;
}
const pointerButtons: readonly PointerButton[] = ['left', 'middle', 'right'];
const DOUBLE_CLICK_MS = 500;
const MODIFIER_POINTER_REFUSAL = 'Shift, Ctrl, Alt or Meta held with the pointer cannot be sent: this connection has no key-down state. Nothing was sent.';
controlFrame.addEventListener('pointerdown', event => {
  if (!directReady() || event.isPrimary === false) return;
  const button = pointerButtons[event.button]; if (!button) return;
  consume(event); modifiers.interrupt(); gesture = null;
  (controlFrame as { focus?: (options?: FocusOptions) => void }).focus?.({ preventScroll: true });
  if (anyModifier(event)) { refuse(MODIFIER_POINTER_REFUSAL); return; }
  const reason = refusal(); if (reason) { refuse(reason); return; }
  const point = surfacePoint(event, false); if (!point) return; // letterbox bar: not a Guest pixel
  gesture = { button, ...point, clientX: event.clientX, clientY: event.clientY, shape: displayed!.shape };
  try { (controlFrame as { setPointerCapture?: (id: number) => void }).setPointerCapture?.(event.pointerId); } catch { /* capture is a convenience only */ }
});
controlFrame.addEventListener('pointerup', event => {
  const started = gesture; gesture = null;
  if (!started || !directReady()) return;
  consume(event);
  if (pointerButtons[event.button] !== started.button) return;
  if (anyModifier(event)) { refuse(MODIFIER_POINTER_REFUSAL); return; }
  if (!displayed || !sameShape(displayed.shape, started.shape)) { refuse('The picture changed during the gesture. Nothing was sent.'); return; }
  const at = performance.now();
  if (Math.hypot(event.clientX - started.clientX, event.clientY - started.clientY) < DRAG_THRESHOLD_CSS_PX) {
    // Pointer events carry no click count. A second press of the same button at the same place within the usual
    // double-click interval is the second click of a double-click.
    const second = !!lastClick && lastClick.button === started.button && at - lastClick.at <= DOUBLE_CLICK_MS && Math.hypot(event.clientX - lastClick.clientX, event.clientY - lastClick.clientY) < DRAG_THRESHOLD_CSS_PX;
    lastClick = { at, button: started.button, clientX: event.clientX, clientY: event.clientY };
    enqueue({ kind: 'click', at, shape: started.shape, x: started.x, y: started.y, button: started.button, second }, 'click');
    return;
  }
  // A drag may end outside the picture; its end is pinned to the nearest edge pixel.
  const end = surfacePoint(event, true); if (!end) return;
  enqueue({ kind: 'drag', at, shape: started.shape, x: started.x, y: started.y, end_x: end.x, end_y: end.y, button: started.button }, 'drag');
});
controlFrame.addEventListener('pointercancel', () => { gesture = null; });
controlFrame.addEventListener('contextmenu', event => { if (directReady()) consume(event); });
controlFrame.addEventListener('wheel', event => {
  if (!directReady()) return;
  consume(event); modifiers.interrupt();
  if (anyModifier(event)) { refuse(MODIFIER_POINTER_REFUSAL); return; }
  const point = surfacePoint(event, false); if (!point) return;
  const step = wheel.add(event.deltaX, event.deltaY, event.deltaMode); if (!step) return;
  enqueue({ kind: 'scroll', at: performance.now(), shape: displayed!.shape, ...point, ...step }, 'scroll');
}, { passive: false });
controlFrame.addEventListener('keydown', event => {
  if (!directReady()) return; // never trap Tab on a surface that cannot send
  // Ctrl+Alt+Escape leaves the surface instead of being sent, so keyboard users are never trapped by Tab capture.
  if (event.ctrlKey && event.altKey && event.code === 'Escape') { consume(event); releaseAll('keyboard focus left the picture'); (controlFrame as { blur?: () => void }).blur?.(); return; }
  // A dead key is still one physical position and is sent as such; the Guest layout composes it. An IME is not.
  if (event.isComposing || event.key === 'Process') { refuse('Text composition (IME) cannot be sent: this connection carries physical keys only. Nothing was sent.'); return; }
  if (modifiers.press(event.code, event.repeat === true)) { consume(event); return; }
  const chord = keyChord(event.code, modifiers.held(event));
  if (!chord) { refuse(`The key ${event.code || 'pressed'} is not available on this connection (or too many modifiers are held). Nothing was sent.`); return; }
  consume(event);
  enqueue({ kind: 'key', at: performance.now(), keys: chord, repeat: event.repeat === true }, 'key');
});
controlFrame.addEventListener('keyup', event => {
  if (!directReady()) { modifiers.clear(); return; }
  const tapped = modifiers.release(event.code);
  if (tapped) { consume(event); enqueue({ kind: 'key', at: performance.now(), keys: tapped, repeat: false }, 'key'); }
});
controlFrame.addEventListener('focus', () => {
  surfaceFocused = true;
  if (directReady() && !displayedFresh() && !pumping && controlReady()) void refreshFrame();
});
controlFrame.addEventListener('blur', () => { surfaceFocused = false; releaseAll('keyboard focus left the picture'); });
window.addEventListener('blur', () => { releaseAll('the window lost focus'); });
(document as { addEventListener?: Document['addEventListener'] }).addEventListener?.('visibilitychange', () => { if (document.visibilityState === 'hidden') releaseAll('the page was hidden'); });
directControl.addEventListener('change', () => { dragStart = null; releaseAll('the input mode was changed'); updateControlControls(); });
updateControlControls();

async function post(path: string, body: unknown, signal: AbortSignal): Promise<Record<string, unknown>> {
  const response = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body), signal, credentials: 'omit', cache: 'no-store' });
  const text = await response.text();
  if (text.length > 256 * 1024) throw new CodecError('response_too_large', 'Native service response exceeded 256KiB.');
  let result: Record<string, unknown>;
  try { result = text ? JSON.parse(text) as Record<string, unknown> : {}; }
  catch { throw new CodecError('native_service_error', `Native service HTTP ${response.status}: ${text.slice(0, 512)}`); }
  if (!response.ok) throw new CodecError('native_service_error', typeof result.error === 'string' ? result.error : `Native service HTTP ${response.status}.`);
  return result;
}
function reasonText(error: unknown): string { return error instanceof Error ? error.message : String(error); }
class DtlsTransportError extends CodecError {
  override readonly code = 'dtls_transport_closed';
  constructor(public readonly state: 'closed' | 'failed', public readonly elapsedMs: number) {
    super('dtls_transport_closed', `Native WebRTC DTLS transport ${state} at ${elapsedMs}ms since connect.`);
  }
}
function waitForIce(pc: RTCPeerConnection, signal: AbortSignal, timeoutMs: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const done = (error?: unknown) => { clearTimeout(timer); pc.removeEventListener('icegatheringstatechange', change); signal.removeEventListener('abort', abort); error ? reject(error) : resolve(); };
    const change = () => { if (pc.iceGatheringState === 'complete') done(); };
    const abort = () => done(signal.reason ?? new CodecError('cancelled', 'Cancelled.'));
    const timer = setTimeout(() => done(new CodecError('ice_gather_timeout', 'ICE gathering exceeded its bounded deadline.')), timeoutMs);
    pc.addEventListener('icegatheringstatechange', change); signal.addEventListener('abort', abort, { once: true });
    if (signal.aborted) abort(); else change();
  });
}
async function nativeAttempt(preferred: VideoCodec, signal: AbortSignal): Promise<AttemptResult> {
  // Cleanup must finish before a fallback can reserve the server's single native worker.
  try { await cleanup; } catch { throw new CodecError('cleanup_unconfirmed', 'Session cleanup is unconfirmed. Reconnect remains blocked.'); }
  signal.throwIfAborted();
  if (negotiationSourceFailure) throw new SourceFailureError(negotiationSourceFailure);
  const attemptController = controller;
  const attemptAuthentication = authenticationRequired ? authenticated : null;
  let rtcConfiguration: RTCConfiguration = { iceServers: [] };
  if (iceConfigurationEnabled) {
    if (!attemptAuthentication) throw new CodecError('ice_authentication_required', 'Configured ICE requires a verified pairing grant.');
    const temporary = await attemptAuthentication.iceConfiguration(signal);
    rtcConfiguration = icePeerConfiguration(temporary);
    show({ iceConfiguration: iceConfigurationSummary(temporary) });
  }
  signal.throwIfAborted();
  const relayRequired = rtcConfiguration.iceTransportPolicy === 'relay';
  let pc: RTCPeerConnection;
  try { pc = new RTCPeerConnection(rtcConfiguration); }
  catch { throw new CodecError('ice_configuration_failed', 'Peer connection rejected its validated configuration.'); }
  // Release our configuration reference; the browser owns its in-memory copy.
  rtcConfiguration = { iceServers: [] };
  const binding: { request_id: string; session_id?: string } = { request_id: crypto.randomUUID() };
  activeRequest = binding;
  let offered = false, closed = false, succeeded = false, checking = false;
  let controlChannel: EncryptedControlChannel | null = null;
  let authenticatedOffer: Promise<unknown> | null = null;
  let authenticatedConfirmation: Promise<void> | null = null;
  let confirmationController: AbortController | undefined;
  let frameCallback: number | undefined, interval: ReturnType<typeof setInterval> | undefined;
  let deadline: ReturnType<typeof setTimeout> | undefined;
  let receptionDeadline: ReceptionDeadline | undefined;
  let lastHashAt = -Infinity;
  let closePromise: Promise<void> | null = null;
  let mediaFailure: MediaFailure | null = null;
  let dtlsFailure: DtlsTransportError | undefined;
  const dtlsListeners = new Map<RTCDtlsTransport, () => void>();
  let failReady: (error: unknown) => void = () => {};
  let presentedFrames = 0, firstPresentedMs: number | null = null, firstDecodedMs: number | null = null;
  const started = performance.now();
  const hashes: string[] = [];
  const iceDiagnostic: Record<string, unknown> = { codec: preferred, relayRequired, localCandidateEvents: iceCandidateCounts(''), candidateErrorCodes: [] };
  const icePhases: Record<string, unknown>[] = [];
  const previousIceAttempts = Array.isArray(report.iceAttempts) ? (report.iceAttempts as unknown[]).slice(-1) : [];
  const publishIce = () => show({ iceAttempts: [...previousIceAttempts, { ...iceDiagnostic, phases: [...icePhases] }] });
  const phase = (name: string) => {
    if (closed) return;
    if (icePhases.length < 24) icePhases.push({ phase: name, elapsedMs: performance.now() - started, gathering: pc.iceGatheringState, ice: pc.iceConnectionState, connection: pc.connectionState });
    publishIce();
  };
  const iceStateChange = () => phase('state-change');
  const candidate = (event: RTCPeerConnectionIceEvent) => {
    if (closed) return;
    if (event.candidate) {
      const addition = iceCandidateCounts(`a=${event.candidate.candidate}`), counts = iceDiagnostic.localCandidateEvents as ReturnType<typeof iceCandidateCounts>;
      for (const key of Object.keys(counts) as (keyof typeof counts)[]) counts[key] += addition[key];
    } else iceDiagnostic.candidateEventsComplete = true;
    publishIce();
  };
  const candidateError = (event: Event) => {
    if (closed) return;
    const code = (event as RTCPeerConnectionIceErrorEvent).errorCode, codes = iceDiagnostic.candidateErrorCodes as number[];
    if (Number.isInteger(code) && code >= 300 && code <= 799 && codes.length < 8) codes.push(code);
    publishIce();
  };
  pc.addEventListener('icecandidate', candidate); pc.addEventListener('icecandidateerror', candidateError);
  pc.addEventListener('iceconnectionstatechange', iceStateChange); pc.addEventListener('icegatheringstatechange', iceStateChange);
  phase('peer-created');
  const sample = document.createElement('canvas'); sample.width = 16; sample.height = 9;
  const pixels = sample.getContext('2d', { willReadFrequently: true })!;
  const poster = lastFrame.getContext('2d')!;
  const drawPoster = () => {
    if (video.videoWidth > 0 && video.videoHeight > 0 && (lastFrame.width !== video.videoWidth || lastFrame.height !== video.videoHeight)) {
      lastFrame.width = video.videoWidth; lastFrame.height = video.videoHeight;
      lastFrame.style.aspectRatio = `${video.videoWidth} / ${video.videoHeight}`;
      video.style.aspectRatio = lastFrame.style.aspectRatio;
    }
    poster.drawImage(video, 0, 0, lastFrame.width, lastFrame.height);
  };
  video.hidden = false; lastFrame.hidden = true;
  const disconnectGrace = new DisconnectGrace(reason => fail(new CodecError('transport_failed', reason === 'disconnected_grace_expired'
    ? `Native WebRTC transport stayed disconnected for ${DISCONNECT_GRACE_MS}ms.`
    : 'Native WebRTC transport failed.')));
  const close = () => {
    if (closePromise) return closePromise;
    phase('cleanup-started');
    closed = true;
    for (const [transport, listener] of dtlsListeners) transport.removeEventListener('statechange', listener);
    dtlsListeners.clear();
    pc.removeEventListener('icecandidate', candidate); pc.removeEventListener('icecandidateerror', candidateError);
    pc.removeEventListener('iceconnectionstatechange', iceStateChange); pc.removeEventListener('icegatheringstatechange', iceStateChange);
    endControlSession();
    clearInterval(interval); clearTimeout(deadline); receptionDeadline?.close(); disconnectGrace.close();
    if (frameCallback !== undefined) video.cancelVideoFrameCallback(frameCallback);
    // After success the poster is no longer redrawn per frame; keep the final frame.
    if (succeeded) { try { drawPoster(); } catch { /* the last per-frame poster remains */ } }
    controlChannel?.close();
    pc.close();
    if (video.srcObject instanceof MediaStream) video.srcObject.getTracks().forEach(track => track.stop());
    video.srcObject = null;
    if (presentedFrames > 0) { video.hidden = true; lastFrame.hidden = false; }
    closePromise = (async () => {
      if (offered) {
        if (authenticationRequired) {
          if (!attemptAuthentication || !authenticatedOffer) throw new CodecError('auth_cleanup_unconfirmed', 'Authenticated negotiation cleanup is unconfirmed.');
          // A rejected confirmation still leaves a verified transcript that can
          // authorize close. Wait for its finally block to release the helper.
          // Local pre-offer failures have no host reservation; transmitted offers
          // still require the verified transcript or remain conservatively fenced.
          await authenticatedOffer.catch(() => {});
          await authenticatedConfirmation?.catch(() => {});
          await pendingControl?.catch(() => {});
          await pendingFile?.catch(() => {});
          // The request is signed; this response is not a separately signed receipt.
          const offerState = attemptAuthentication.offerTransmissionState;
          const cleanupBasis = offerState === 'not-sent' ? 'local-offer-not-sent' : offerState === 'rejected' ? 'signed-offer-rejection' : 'close-response-to-signed-request';
          mediaFailure = await attemptAuthentication.close(AbortSignal.timeout(15000));
          show({ cleanupBasis });
        } else {
          const receipt = await post('/close', binding, AbortSignal.timeout(5000));
          if (receipt.closed !== true) throw new CodecError('cleanup_unconfirmed', 'Session cleanup was not acknowledged.');
          mediaFailure = mediaFailureFromReceipt(receipt);
        }
      } else if (attemptAuthentication) {
        await attemptAuthentication.close();
        show({ cleanupBasis: 'local-offer-not-sent' });
      }
      if (mediaFailure) {
        negotiationSourceFailure = mediaFailure;
        const message = new SourceFailureError(mediaFailure).message;
        show({ mediaFailure, sourceFailureMessage: message });
        if (succeeded) { show({ status: 'stream-ended', error: message }); status.textContent = message; setLink('ended', message); }
      }
      if (activeRequest === binding) activeRequest = null;
    })();
    return closePromise;
  };
  const beginCleanup = () => {
    const current = close(); cleanup = current; cleanupState = 'pending'; updateConnectionControls();
    void current.then(() => {
      if (cleanup !== current) return;
      cleanupState = 'idle';
      if (closeCurrent === beginCleanup) fileStatus.textContent = 'Disconnected. Session file cleanup acknowledged.';
      // After first-frame acceptance, the page owns this session independently
      // of negotiation. Automatic termination must release that ownership too.
      // An older close completion must never reset a newer attempt's controller.
      if (succeeded && controller === attemptController && closeCurrent === beginCleanup) {
        controller = null; disconnect.disabled = true;
        closeCurrent = null; retryCurrentCleanup = null;
        show({ cleanupStatus: 'acknowledged' });
        if (report.status === 'probe-deadline') status.textContent = '120-second diagnostic deadline reached. Session cleanup acknowledged; ready to reconnect.';
      }
      updateConnectionControls();
    }, error => {
      if (cleanup !== current) return;
      cleanupState = 'failed'; updateConnectionControls();
      if (closeCurrent === beginCleanup) fileStatus.textContent = 'Disconnected. File cleanup is unconfirmed. Reconnect remains blocked until cleanup is acknowledged.';
      show({ cleanupError: String(error), cleanupStatus: 'unconfirmed' });
    });
    return current;
  };
  retryCurrentCleanup = () => {
    if (cleanupState !== 'failed') throw new CodecError('cleanup_not_retryable', 'No failed cleanup to retry.');
    // Explicitly retry this closure's original request and verified transcript.
    // Never reset the global fence or create another offer before its receipt.
    closePromise = null;
    return beginCleanup();
  };
  closeCurrent = beginCleanup;
  const abort = () => { failReady(signal.reason ?? new CodecError('cancelled', 'Cancelled.')); void beginCleanup().catch(() => {}); };
  signal.addEventListener('abort', abort, { once: true });
  const fail = (error: unknown) => {
    if (closed) return;
    show({ attemptFailure: String(error) });
    failReady(error);
    if (succeeded) { show({ status: 'stream-ended', error: String(error) }); status.textContent = 'Stream ended; see result.'; setLink('ended', reasonText(error)); }
    void beginCleanup().catch(() => {});
  };
  const observeDtls = (receiver?: RTCRtpReceiver) => {
    if (closed) return;
    const endpoints: (RTCRtpReceiver | RTCRtpSender | undefined)[] = [receiver];
    try { endpoints.push(...(pc.getReceivers?.() ?? [])); } catch { /* transport discovery is optional */ }
    try { endpoints.push(...(pc.getSenders?.() ?? [])); } catch { /* transport discovery is optional */ }
    try {
      for (const transceiver of pc.getTransceivers?.() ?? []) endpoints.push(transceiver.receiver, transceiver.sender);
    } catch { /* transport discovery is optional */ }
    for (const endpoint of endpoints) {
      try {
        const transport = endpoint?.transport;
        if (!transport || dtlsListeners.has(transport)) continue;
        const change = () => {
          if (closed || pc.connectionState === 'failed' || pc.connectionState === 'closed') return;
          if (transport.state !== 'closed' && transport.state !== 'failed') return;
          dtlsFailure = new DtlsTransportError(transport.state, performance.now() - started);
          show({ dtlsTransportFailure: { code: dtlsFailure.code, state: dtlsFailure.state, elapsedMs: dtlsFailure.elapsedMs } });
          confirmationController?.abort(dtlsFailure);
          fail(dtlsFailure);
        };
        dtlsListeners.set(transport, change);
        transport.addEventListener('statechange', change);
        change();
        if (closed) return;
      } catch { /* an unavailable endpoint must not prevent observing the others */ }
    }
  };
  pc.onconnectionstatechange = () => {
    if (closed) return;
    phase('connection-state-change'); receptionDeadline?.update();
    const waiting = disconnectGrace.pending;
    disconnectGrace.update(pc.connectionState);
    if (disconnectGrace.pending !== waiting) {
      phase(waiting ? 'disconnect-grace-cancelled' : 'disconnect-grace-started');
      setLink(waiting ? (succeeded ? 'live' : 'connecting') : 'reconnecting');
    }
    observeDtls();
  };
  pc.ontrack = event => {
    if (closed) return;
    observeDtls(event.receiver);
    if (closed) return;
    phase('track-received');
    // Ask for the smallest jitter buffer. Browsers without a property are unaffected.
    const receiver = event.receiver as (RTCRtpReceiver & { jitterBufferTarget?: number | null; playoutDelayHint?: number | null }) | undefined;
    const appliedHints: string[] = [];
    for (const hint of ['jitterBufferTarget', 'playoutDelayHint'] as const) {
      if (!receiver || !(hint in receiver)) continue;
      try { receiver[hint] = 0; appliedHints.push(hint); } catch { /* an unsupported value leaves the browser default */ }
    }
    show({ receiverLatencyHints: { requestedValue: 0, applied: appliedHints } });
    video.srcObject = new MediaStream([event.track]);
    void video.play().catch(error => fail(new CodecError('playback_failed', String(error))));
    const draw: VideoFrameRequestCallback = () => {
      if (closed) return;
      try {
        presentedFrames++; firstPresentedMs ??= performance.now() - started;
        if (presentedFrames === 1) phase('first-frame-presented');
        // Until success every frame is copied and hashed as acceptance evidence.
        // Afterwards the poster is taken once at close, hashing stops once two
        // distinct hashes prove pixel change, and a static picture is sampled
        // at most once per second.
        if (!succeeded) drawPoster();
        if (!succeeded || (hashes.length < 2 && performance.now() - lastHashAt >= 1000)) {
          lastHashAt = performance.now();
          pixels.drawImage(video, 0, 0, 16, 9);
          let value = 2166136261;
          for (const byte of pixels.getImageData(0, 0, 16, 9).data) value = Math.imul(value ^ byte, 16777619) >>> 0;
          const hash = value.toString(16).padStart(8, '0'); if (!hashes.includes(hash) && hashes.length < 8) hashes.push(hash);
        }
        frameCallback = video.requestVideoFrameCallback(draw);
      } catch (error) { fail(error); }
    };
    frameCallback = video.requestVideoFrameCallback(draw);
  };
  try {
    if (typeof video.requestVideoFrameCallback !== 'function') throw new CodecError('presentation_callback_unavailable', 'Receiver presentation validation requires requestVideoFrameCallback.');
    if ((controlEnabled || filesEnabled) && attemptAuthentication) {
      controlChannel = new EncryptedControlChannel(
        pc.createDataChannel('veynode-control-v1', { ordered: true, protocol: 'veynode-control/v1' }),
        { isConfirmed: () => attemptAuthentication.controlConfirmed, onFailure: fail },
      );
      attemptAuthentication.bindControlTransport(
        (signed, operationSignal) => controlChannel!.request(signed, operationSignal),
        () => controlChannel!.close(),
      );
      show({ controlTransport: 'authenticated-webrtc-datachannel-v1' });
    }
    const transceiver = pc.addTransceiver('video', { direction: 'recvonly' });
    transceiver.setCodecPreferences(nativeCodecPreferences(RTCRtpReceiver.getCapabilities('video')?.codecs ?? [], preferred));
    phase('local-description-started');
    await pc.setLocalDescription(await pc.createOffer());
    await waitForIce(pc, signal, iceConfigurationEnabled ? 10000 : 3000);
    iceDiagnostic.localSdp = iceCandidateCounts(pc.localDescription!.sdp); phase('local-gathered');
    signal.throwIfAborted();
    offered = true;
    let answer: Record<string, unknown>;
    if (authenticationRequired) {
      if (!attemptAuthentication || !grantReady) throw new CodecError('pairing_required', 'Verify the local approved grant first.');
      // Keep the bounded request alive after UI cancellation so its signed answer
      // can authorize cleanup; no unsigned request/session cancellation fallback.
      const pending = attemptAuthentication.offer(pc.localDescription!.sdp, binding.request_id, preferred, AbortSignal.timeout(10000));
      authenticatedOffer = pending;
      answer = (await pending).answer;
    } else answer = await post('/offer', { ...binding, type: 'offer', sdp: pc.localDescription!.sdp, codec: preferred }, signal);
    if (answer.type !== 'answer' || typeof answer.sdp !== 'string' || typeof answer.session_id !== 'string') throw new CodecError('invalid_answer', 'Native service returned an invalid answer/session binding.');
    iceDiagnostic.remoteSdp = iceCandidateCounts(answer.sdp); phase('signed-answer-received');
    binding.session_id = answer.session_id;
    signal.throwIfAborted();
    await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
    observeDtls();
    // Track events can expose a terminal transport before the frame promise exists.
    if (dtlsFailure) throw dtlsFailure;
    phase('remote-description-installed');
    if (authenticationRequired) {
      confirmationController = new AbortController();
      const abortConfirmation = () => confirmationController!.abort(signal.reason);
      signal.addEventListener('abort', abortConfirmation, { once: true });
      if (signal.aborted) abortConfirmation();
      try {
        authenticatedConfirmation = attemptAuthentication!.confirm(confirmationController.signal);
        await authenticatedConfirmation;
      } catch (error) { throw dtlsFailure ?? error; }
      finally {
        signal.removeEventListener('abort', abortConfirmation);
        confirmationController = undefined;
      }
      if (dtlsFailure) throw dtlsFailure;
      phase('signed-confirmation-acknowledged');
      if (!closed && !signal.aborted) { controlReceiver = attemptAuthentication; controlSessionReady = true;
        if (filesEnabled && requestedScopes.includes('files') && attemptAuthentication) {
          const receiver = attemptAuthentication;
          sessionFiles = new SessionFiles(async (operation, signal) => (await receiver.control({ kind: 'file', operation }, signal)).result);
          fileStatus.textContent = 'Files ready in the configured Guest folder. Maximum 8 MiB; existing files are never replaced.';
        }
        updateControlControls(); }
    }
    const result = await new Promise<AttemptResult>((resolve, reject) => {
      failReady = reject;
      if (dtlsFailure) { reject(dtlsFailure); return; }
      if (signal.aborted) { abort(); return; }
      receptionDeadline = new ReceptionDeadline({
        connected: () => pc.connectionState === 'connected',
        confirmed: () => !authenticationRequired || attemptAuthentication?.controlConfirmed === true,
        onPhase: phase,
        onTimeout: code => fail(new CodecError(code, code === 'connection_timeout'
          ? 'Native WebRTC connection did not establish within 12000ms after confirmation.'
          : 'Native stream did not produce a decoded/presented frame within 6000ms after connection and confirmation.')),
      });
      const pollStats = () => {
        if (checking || closed) return;
        checking = true;
        void (async () => {
          const stats = await pc.getStats();
          if (closed) return;
          batchReport(() => {
            receptionDeadline?.update();
            let inbound: RTCInboundRtpStreamStats | undefined;
            const controlChannels: Record<string, unknown>[] = [], transports: Record<string, unknown>[] = [];
            stats.forEach(stat => {
              if (stat.type === 'inbound-rtp' && stat.kind === 'video' && stat.framesDecoded > 0) inbound = stat as RTCInboundRtpStreamStats;
              if (controlChannel && stat.type === 'data-channel' && stat.label === 'veynode-control-v1') controlChannels.push({ label: stat.label, protocol: stat.protocol, state: stat.state, messagesSent: stat.messagesSent, messagesReceived: stat.messagesReceived, bytesSent: stat.bytesSent, bytesReceived: stat.bytesReceived });
              if (stat.type === 'transport') transports.push({ dtlsState: stat.dtlsState, iceState: stat.iceState });
            });
            if (controlChannel) show({ controlTransportStats: { channels: controlChannels, transports } });
            iceDiagnostic.stats = iceStatsCounts(stats);
            const reception = videoReceptionCounts(stats), elapsedMs = performance.now() - started;
            iceDiagnostic.videoReception = reception;
            iceDiagnostic.videoElement = { readyState: video.readyState, width: video.videoWidth, height: video.videoHeight, presentedFrames };
            if (transports.some(value => value.dtlsState === 'connected')) iceDiagnostic.firstDtlsConnectedMs ??= elapsedMs;
            if ((reception.packetsReceived ?? 0) > 0) iceDiagnostic.firstRtpReceivedMs ??= elapsedMs;
            if ((reception.framesDecoded ?? 0) > 0) iceDiagnostic.firstDecodedFrameMs ??= elapsedMs;
            const icePath = selectedIcePath(stats);
            // Keep historical, sanitized path evidence when disconnection removes
            // the current selected pair. It must never substitute for current ICE
            // evidence in the forced-relay acceptance gate below.
            if (icePath) iceDiagnostic.lastSelectedPath = { observedElapsedMs: elapsedMs, path: icePath };
            publishIce();
            show({ selectedIcePath: icePath });
            if (relayRequired && icePath && icePath.localCandidateType !== 'relay') throw new CodecError('relay_policy_violation', 'Forced relay did not select a local relay candidate.');
            // Missing candidate evidence must never be reported as a forced TURN pass.
            if (relayRequired && !icePath) return;
            if (!inbound?.codecId || presentedFrames < 1) return;
            const codec = stats.get(inbound.codecId);
            if (!codec) return;
            if (firstDecodedMs === null) {
              if (!receptionDeadline?.firstFrame()) return;
              firstDecodedMs = performance.now() - started;
              clearTimeout(deadline);
              deadline = setTimeout(() => fail(new CodecError('frame_progress_timeout', 'No five decoded/presented native frames within 6000ms after first frame.')), 6000);
            }
            const detail = { codec: { mimeType: codec.mimeType, payloadType: codec.payloadType, sdpFmtpLine: codec.sdpFmtpLine ?? null }, framesDecoded: inbound.framesDecoded, presentedFrames, distinctSampledPixelHashes: [...hashes], pixelChangeObserved: hashes.length >= 2, pixelSample: { width: 16, height: 9, algorithm: 'FNV1a32 RGBA (change detector only)' }, pixelSamplingComplete: succeeded && hashes.length >= 2, firstDecodedMs, firstPresentedMs, frameWidth: inbound.frameWidth, frameHeight: inbound.frameHeight, framesDropped: inbound.framesDropped ?? null, bytesReceived: inbound.bytesReceived, connectionState: pc.connectionState };
            const selected = codecName(String(codec.mimeType));
            if (!selected || (preferred === 'h264' && selected !== 'h264') || host?.[selected] !== 'verified-hardware') throw new CodecError('unexpected_codec', 'Native sender used a codec outside the verified hardware policy.');
            nativeCodecPreferences([{ mimeType: codec.mimeType, clockRate: 90000, sdpFmtpLine: codec.sdpFmtpLine ?? '' }, ...(selected === 'h265' ? nativeCodecPreferences(RTCRtpReceiver.getCapabilities('video')?.codecs ?? [], 'h264') : [])], selected);
            show({ latestStats: { ...detail, feedbackNegotiation: {
              offered: videoFeedbackSummary(pc.localDescription?.sdp, codec.payloadType),
              answered: videoFeedbackSummary(pc.remoteDescription?.sdp, codec.payloadType),
            } } });
            if (succeeded) return;
            if (inbound.framesDecoded! < 5 || presentedFrames < 5) return;
            succeeded = true;
            clearTimeout(deadline);
            // Acceptance evidence is complete: keep the report current at a tenth of the cost.
            clearInterval(interval); interval = setInterval(pollStats, 1000);
            // Successful session ownership transfers to the page, outside the attempt deadline.
            signal.removeEventListener('abort', abort);
            deadline = setTimeout(() => {
              show({ status: 'probe-deadline', endedAt: new Date().toISOString() }); status.textContent = '120-second diagnostic deadline reached; closing the session…';
              setLink('ended', 'the 120-second diagnostic session limit was reached.');
              void beginCleanup().catch(() => {});
            }, 120000);
            resolve({ codec: codec.mimeType, framesDecoded: inbound.framesDecoded!, detail });
          });
        })().catch(fail).finally(() => { checking = false; });
      };
      interval = setInterval(pollStats, 100);
    });
    return result;
  } finally {
    signal.removeEventListener('abort', abort);
    if (!succeeded) {
      try { await beginCleanup(); } catch { throw new CodecError('cleanup_unconfirmed', 'Session cleanup is unconfirmed. Reconnect remains blocked.'); }
      if (mediaFailure && !signal.aborted) throw new SourceFailureError(mediaFailure);
    }
  }
}

async function loadCapabilities() {
  const response = await fetch('/capabilities', { cache: 'no-store', credentials: 'omit', signal: AbortSignal.timeout(8000) });
  if (!response.ok) throw new Error(`Native capability probe returned HTTP ${response.status}.`);
  const value = await response.json() as Record<string, unknown>;
  const allowed = ['verified-hardware', 'unavailable', 'unverified'];
  if (!allowed.includes(String(value.h264)) || !allowed.includes(String(value.h265))) throw new Error('Native hardware capability schema invalid.');
  host = { h264: value.h264 as HostCapabilities['h264'], h265: value.h265 as HostCapabilities['h265'] };
  show({ status: 'ready', hostCapabilityReport: { h264: host.h264, h265: host.h265 }, receiverCapabilities: RTCRtpReceiver.getCapabilities('video'), scope: 'localhost-native-vm-webrtc', internetRemoteValidated: false });
  const auth = value.authentication as { required?: boolean; vm_id?: string; control_enabled?: boolean; files_enabled?: boolean; control_transport?: string; ice_config_enabled?: boolean } | undefined;
  authenticationRequired = auth?.required === true;
  iceConfigurationEnabled = auth?.ice_config_enabled === true;
  if (iceConfigurationEnabled && !authenticationRequired) throw new Error('Configured ICE requires authenticated signaling.');
  show({ authentication: { required: authenticationRequired, controlEnabled: auth?.control_enabled === true, filesEnabled: auth?.files_enabled === true, iceConfigurationEnabled } });
  controlEnabled = authenticationRequired && auth?.control_enabled === true;
  filesEnabled = authenticationRequired && auth?.files_enabled === true;
  requestFiles.disabled = !filesEnabled;
  if ((controlEnabled || filesEnabled) && auth?.control_transport !== 'webrtc-datachannel-v1') throw new Error('Encrypted console control transport is unavailable.');
  requestInput.disabled = !controlEnabled; updateControlControls();
  if (authenticationRequired) {
    if (typeof auth?.vm_id !== 'string') throw new Error('Authenticated service VM binding missing.');
    authVmId = auth.vm_id; document.querySelector<HTMLElement>('#pairing')!.hidden = false;
    status.textContent = 'Local signed session requires a verified pairing grant.'; connect.disabled = true;
  } else { status.textContent = 'Native hardware probe loaded. Unsigned localhost diagnostic ready.'; connect.disabled = false; }
}
void loadCapabilities().catch(error => { show({ status: 'blocked', error: String(error) }); status.textContent = 'Native service unavailable or incompatible; no fixture capability substituted.'; });
connect.addEventListener('click', async () => {
  if (controller || !host || cleanupState !== 'idle' || (authenticationRequired && !grantReady)) return;
  negotiationSourceFailure = null;
  controller = new AbortController(); connect.disabled = true; disconnect.disabled = false;
  status.textContent = 'Negotiating native console…'; inputNotice.textContent = ''; setLink('connecting');
  report = { hostCapabilityReport: report.hostCapabilityReport, receiverCapabilities: report.receiverCapabilities, scope: 'localhost-native-vm-webrtc', internetRemoteValidated: false };
  show({ iceAttempts: [], status: 'connecting', startedAt: new Date().toISOString(), userAgent: navigator.userAgent });
  try {
    const plan = createNativeCodecPlan(host, RTCRtpReceiver.getCapabilities('video')?.codecs ?? []);
    const preference = document.querySelector<HTMLSelectElement>('#codec-preference')!.value;
    if (preference === 'h264') { plan.preferred = 'h264'; plan.reason = 'explicit_h264_test'; }
    show({ requestedPreference: preference });
    const result = await negotiateCodec(plan, nativeAttempt, controller.signal, iceConfigurationEnabled ? 30000 : 12000);
    show({ status: 'passed-live', policyReason: plan.reason, ...result, verifiedAt: new Date().toISOString() });
    status.textContent = `Receiving native ${result.selected.toUpperCase()} (${result.fallbackReason ?? 'preferred codec'}). Disconnect ends this session.`;
    if (linkState === 'connecting') setLink('live');
  } catch (error) {
    show({ status: 'failed', error: String(error), attempts: error instanceof NegotiationError ? error.attempts : [] }); status.textContent = error instanceof NegotiationError && error.code.startsWith('guest_') ? error.message : 'Native stream failed or cancelled.';
    controller = null; updateConnectionControls(); disconnect.disabled = true;
    // An explicit disconnect during negotiation has already recorded its own reason.
    if (linkState !== 'ended') setLink('ended', error instanceof NegotiationError && error.code.startsWith('guest_') ? error.message : `the connection could not be established (${reasonText(error)})`);
  }
});
disconnect.addEventListener('click', async () => {
  controller?.abort(); disconnect.disabled = true;
  if (linkState !== 'idle' && linkState !== 'ended') setLink('ended', 'you disconnected.');
  try { await closeCurrent?.(); show({ status: 'disconnected', endedAt: new Date().toISOString() }); status.textContent = 'Native stream disconnected.'; }
  catch (error) { show({ status: 'cleanup-failed', error: String(error) }); status.textContent = 'Close could not be confirmed. Retry cleanup for this session; reconnect remains blocked until acknowledged.'; }
  finally { controller = null; updateConnectionControls(); }
});
retryCleanup.addEventListener('click', async () => {
  if (cleanupState !== 'failed' || !retryCurrentCleanup) return;
  status.textContent = 'Retrying cleanup of the same session…';
  try {
    await retryCurrentCleanup();
    show({ status: 'disconnected', cleanupStatus: 'acknowledged', cleanupRecoveredAt: new Date().toISOString() });
    status.textContent = 'Session cleanup acknowledged. Ready to reconnect.';
  } catch (error) {
    show({ status: 'cleanup-failed', cleanupError: String(error), cleanupStatus: 'unconfirmed' });
    status.textContent = 'Cleanup is still unconfirmed. Reconnect remains blocked; no new session was created.';
  } finally { updateConnectionControls(); }
});
window.addEventListener('pagehide', () => {
  controller?.abort();
  if (activeRequest && !authenticationRequired) void fetch('/close', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(activeRequest), credentials: 'omit', keepalive: true }).catch(() => {});
});
document.querySelector<HTMLButtonElement>('#download')!.addEventListener('click', () => {
  const url = URL.createObjectURL(new Blob([JSON.stringify(report, null, 2)], { type: 'application/json' }));
  const link = document.createElement('a'); link.href = url; link.download = `native-webrtc-${Date.now()}.json`; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000);
});

const pairingStatus = document.querySelector<HTMLElement>('#pairing-status')!;
document.querySelector<HTMLButtonElement>('#prepare-pairing')!.addEventListener('click', () => {
  if (controller || cleanupState !== 'idle') return;
  connect.disabled = true; grantReady = false;
  void (async () => {
    requestedScopes = [...(requestFiles.checked && filesEnabled ? ['files' as const] : []), ...(requestInput.checked && controlEnabled ? ['input' as const] : []), 'observe'];
    requestInput.disabled = true; requestFiles.disabled = true; resetPairing.disabled = true;
    authenticated = await AuthenticatedReceiver.open({ pinnedHostPublicKey: document.querySelector<HTMLInputElement>('#host-pin')!.value.trim(), vmId: authVmId, databaseName: 'veynode-session-auth-native-test', scopes: requestedScopes });
    const request = await authenticated.pairingRequest();
    document.querySelector<HTMLElement>('#pairing-request')!.textContent = JSON.stringify(request, null, 2);
    document.querySelector<HTMLButtonElement>('#accept-grant')!.disabled = false;
    resetPairing.disabled = false;
    pairingStatus.textContent = `Compare this digest locally before approving ${requestedScopes.join(', ')} permissions.`;
  })().catch(error => {pairingStatus.textContent = String(error);});
});
document.querySelector<HTMLButtonElement>('#accept-grant')!.addEventListener('click', () => {
  if (controller || cleanupState !== 'idle' || !authenticated) return;
  void (async () => {
    await authenticated!.acceptGrant(JSON.parse(document.querySelector<HTMLTextAreaElement>('#signed-grant')!.value) as SignedGrant);
    grantReady = true; updateConnectionControls();
    pairingStatus.textContent = 'Host signature, device, VM and requested permissions verified. Ready.';
  })().catch(error => {grantReady = false; connect.disabled = true; pairingStatus.textContent = String(error);});
});

resetPairing.addEventListener('click', () => {
  if (controller || cleanupState !== 'idle' || pendingControl) return;
  authenticated?.dispose(); authenticated = null; grantReady = false; requestedScopes = ['observe'];
  requestInput.disabled = !controlEnabled; requestFiles.disabled = !filesEnabled; resetPairing.disabled = true;
  document.querySelector<HTMLButtonElement>('#accept-grant')!.disabled = true;
  document.querySelector<HTMLElement>('#pairing-request')!.textContent = '';
  pairingStatus.textContent = 'Choose requested permissions, then prepare a new pairing request.';
  updateConnectionControls();
});

async function runFileTransfer(kind: 'upload' | 'download') {
  const files = sessionFiles;
  if (!files || pendingFile || pendingControl || !controlSessionReady || cleanupState !== 'idle') return;
  const selected = kind === 'upload' ? uploadFile.files?.[0] : undefined;
  if (kind === 'upload' && (!selected || selected.size > FILE_MAX_BYTES)) { fileStatus.textContent = 'Choose one file of at most 8 MiB.'; return; }
  const operation = (async () => {
    fileStatus.textContent = kind === 'upload' ? 'Uploading…' : 'Downloading…';
    if (kind === 'upload') {
      const result = await files.upload(selected!.name, selected!);
      if (sessionFiles !== files) return;
      uploadFile.value = '';
      fileStatus.textContent = `Upload published: ${result.size} bytes; SHA-256 verified.`;
      show({ fileTransfer: { direction: kind, bytes: result.size, sha256: result.sha256, verified: true } });
    } else {
      const name = document.querySelector<HTMLInputElement>('#download-file-name')!.value;
      const result = await files.download(name);
      try {
        if (sessionFiles !== files) return;
        const url = URL.createObjectURL(new Blob([result.bytes], { type: 'application/octet-stream' }));
        const link = document.createElement('a'); link.href = url; link.download = name; link.click();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
        fileStatus.textContent = `Download verified: ${result.bytes.length} bytes.`;
        show({ fileTransfer: { direction: kind, bytes: result.bytes.length, sha256: result.sha256, verified: true } });
      } finally { result.bytes.fill(0); }
    }
  })();
  pendingFile = operation; updateControlControls();
  try { await operation; } catch (error) {
    if (sessionFiles === files) {
      fileStatus.textContent = error instanceof FileOperationError ? error.message + (error.code === 'durability_unknown' ? '. Publication may have occurred; do not retry automatically.' : '') + (error.cleanupUnconfirmed ? ' Cleanup is unconfirmed; closing the session.' : '') : 'Transfer failed. Verify session cleanup before retrying.';
      show({ fileTransfer: { direction: kind, verified: false, error: error instanceof FileOperationError ? error.code : 'transfer_failed', cleanupUnconfirmed: error instanceof FileOperationError && error.cleanupUnconfirmed } });
      if (error instanceof FileOperationError && error.cleanupUnconfirmed) { controller?.abort(); void closeCurrent?.().catch(() => {}); }
    }
  } finally { if (pendingFile === operation) pendingFile = null; updateControlControls(); }
}
uploadButton.addEventListener('click', () => { void runFileTransfer('upload'); });
downloadButton.addEventListener('click', () => { void runFileTransfer('download'); });
cancelFile.addEventListener('click', () => { sessionFiles?.cancel(); fileStatus.textContent = 'Cancelling after the current bounded chunk…'; });
