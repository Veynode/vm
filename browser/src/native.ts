import { SessionFiles, FileOperationError, FILE_MAX_BYTES } from './session-files.js';
import { icePeerConfiguration, iceConfigurationSummary, selectedIcePath, iceCandidateCounts, iceStatsCounts, videoReceptionCounts } from './ice-config.js';
import { AuthenticatedReceiver } from './native-auth.js';
import { videoFeedbackSummary } from './feedback-evidence.js';
import { EncryptedControlChannel } from './control-channel.js';
import type { SignedGrant, Scope } from './session-auth.js';
import { screenshotAnchor, verifiedScreenshotSource, type FrameAnchor, type ControlAction, type ControlResponse } from './session-control.js';
import { codecName, nativeCodecPreferences, createNativeCodecPlan, negotiateCodec, ReceptionDeadline, mediaFailureFromReceipt, SourceFailureError, type MediaFailure, CodecError, NegotiationError, type HostCapabilities, type AttemptResult, type VideoCodec } from './codec-policy.js';

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
function show(values: Record<string, unknown>) { report = { ...report, ...values }; output.textContent = JSON.stringify(report, null, 2); }
const controlPanel = document.querySelector<HTMLFieldSetElement>('#desktop-control')!;
const controlStatus = document.querySelector<HTMLElement>('#control-status')!;
const controlFrame = document.querySelector<HTMLCanvasElement>('#control-frame')!;
const acquireControl = document.querySelector<HTMLButtonElement>('#acquire-control')!;
const releaseControl = document.querySelector<HTMLButtonElement>('#release-control')!;
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
let lease: { epoch: number; expires: number } | null = null;
let observedFrame: { anchor: FrameAnchor; expires: number } | null = null;
let dragStart: { x: number; y: number } | null = null;
let frameExpiry: ReturnType<typeof setTimeout> | undefined;
let leaseExpiry: ReturnType<typeof setTimeout> | undefined;
function updateControlControls() {
  const available = controlEnabled && controlSessionReady && cleanupState === 'idle' && !pendingControl && !pendingFile;
  const filesAvailable = filesEnabled && requestedScopes.includes('files') && controlSessionReady && cleanupState === 'idle' && !!sessionFiles;
  filePanel.hidden = !filesEnabled;
  uploadButton.disabled = downloadButton.disabled = !filesAvailable || !!pendingFile || !!pendingControl;
  cancelFile.disabled = !filesAvailable || !pendingFile;
  const canInput = available && requestedScopes.includes('input');
  const owns = !!lease && performance.now() < lease.expires;
  const fresh = !!observedFrame && performance.now() < observedFrame.expires;
  controlPanel.hidden = !controlEnabled;
  acquireControl.disabled = !canInput || owns;
  releaseControl.disabled = !canInput || !owns;
  refreshControlFrame.disabled = !available;
  sendPhysicalKey.disabled = !canInput || !owns || !fresh;
  controlFrame.setAttribute('aria-disabled', String(!canInput || !owns || !fresh));
  controlFrame.style.opacity = fresh ? '1' : '0.5';
}
function clearControlLease(message: string) {
  controlGeneration++; lease = null; observedFrame = null; dragStart = null;
  clearTimeout(frameExpiry); clearTimeout(leaseExpiry);
  controlStatus.textContent = message; updateControlControls();
}
function endControlSession() {
  sessionFiles?.dispose(); sessionFiles = null; uploadFile.value = '';
  fileStatus.textContent = 'Disconnected. File cleanup requires the session close acknowledgment.';
  controlSessionReady = false; controlReceiver = null;
  clearControlLease('Control is disconnected. Connect and acquire control explicitly to send input.');
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
    clearControlLease('Control request failed. Refresh and acquire control explicitly before further input.');
    show({ controlError: String(error), controlAction: action.kind, controlCompletedAt: new Date().toISOString(), controlRoundTripMs: performance.now() - requestedAt }); throw error;
  } finally { if (pendingControl === operation) pendingControl = null; updateControlControls(); }
}
function inputState(): { epoch: number; frame: FrameAnchor } | null {
  if (!controlReady() || !requestedScopes.includes('input') || !lease || performance.now() >= lease.expires) return null;
  if (!observedFrame || performance.now() >= observedFrame.expires) {
    observedFrame = null; dragStart = null; controlStatus.textContent = 'Screenshot expired. Refresh it before sending input.'; updateControlControls(); return null;
  }
  return { epoch: lease.epoch, frame: { ...observedFrame.anchor } };
}
async function dispatchInput(action: ControlAction) {
  const generation = controlGeneration;
  try {
    await sendControl(action);
    if (generation !== controlGeneration) return;
    observedFrame = null; dragStart = null; clearTimeout(frameExpiry);
    controlStatus.textContent = 'Input dispatched. Refresh the screenshot to check the result; application delivery is not confirmed.';
    show({ controlAction: action.kind, controlDispatched: true, guestDeliveryConfirmed: false });
  } catch { /* sendControl already fences the lease and records a bounded error. */ }
  finally { updateControlControls(); }
}
acquireControl.addEventListener('click', () => { void (async () => {
  if (!controlReady() || !requestedScopes.includes('input') || (lease && performance.now() < lease.expires)) return;
  const generation = controlGeneration, started = performance.now();
  try {
    const result = await sendControl({ kind: 'acquire' });
    if (generation !== controlGeneration) return;
    lease = { epoch: result.epoch as number, expires: started + (result.expires_in_seconds as number) * 1000 };
    clearTimeout(leaseExpiry); leaseExpiry = setTimeout(() => clearControlLease('Control lease expired. Acquire control explicitly again.'), Math.max(0, lease.expires - performance.now()));
    controlStatus.textContent = 'Control acquired for up to 60 seconds. Refresh the screenshot before input.';
  } catch { /* explicit reacquire remains required */ }
  finally { updateControlControls(); }
})(); });
releaseControl.addEventListener('click', () => { void (async () => {
  if (!controlReady() || !lease || !requestedScopes.includes('input')) return;
  const epoch = lease.epoch;
  clearControlLease('Releasing control…');
  try { await sendControl({ kind: 'release', epoch }); controlStatus.textContent = 'Control released.'; }
  catch { /* failed release remains locally fenced */ }
})(); });
refreshControlFrame.addEventListener('click', () => { void (async () => {
  if (!controlReady()) return;
  const generation = ++controlGeneration, started = performance.now();
  observedFrame = null; dragStart = null; clearTimeout(frameExpiry); updateControlControls();
  try {
    const result = await sendControl({ kind: 'observe_frame' });
    const anchor = screenshotAnchor(result);
    const image = new Image(); image.src = await verifiedScreenshotSource(result);
    await image.decode();
    if (generation !== controlGeneration || !controlSessionReady) return;
    if (image.naturalWidth !== anchor.width || image.naturalHeight !== anchor.height) throw new Error('Decoded screenshot dimensions do not match its input coordinates.');
    if (performance.now() >= started + 4000) throw new Error('Screenshot expired while loading; refresh again.');
    const context = controlFrame.getContext('2d'); if (!context) throw new Error('Screenshot canvas unavailable.');
    controlFrame.width = anchor.width; controlFrame.height = anchor.height; controlFrame.style.aspectRatio = `${anchor.width} / ${anchor.height}`;
    context.drawImage(image, 0, 0, anchor.width, anchor.height); controlFrame.hidden = false;
    observedFrame = { anchor, expires: started + 4000 };
    frameExpiry = setTimeout(() => { observedFrame = null; dragStart = null; controlStatus.textContent = 'Screenshot expired. Refresh it before sending input.'; updateControlControls(); }, Math.max(0, observedFrame.expires - performance.now()));
    controlStatus.textContent = lease ? 'Screenshot ready for 4 seconds. Choose an action and click the screenshot.' : 'Screenshot ready for observation. Acquire control explicitly to send input.';
  } catch (error) { if (generation !== controlGeneration) return; clearControlLease('Screenshot unavailable. Refresh and acquire control explicitly before input.'); show({ controlError: String(error), controlAction: 'observe_frame' }); }
  finally { updateControlControls(); }
})(); });
const physicalKeys = new Set(['ret', 'esc', 'tab', 'backspace', 'delete', 'up', 'down', 'left', 'right', 'home', 'end', 'pgup', 'pgdn', 'spc', 'ctrl,a']);
sendPhysicalKey.addEventListener('click', () => {
  const state = inputState(), selection = document.querySelector<HTMLSelectElement>('#physical-key')!.value;
  if (!state || !physicalKeys.has(selection)) return;
  void dispatchInput({ kind: 'key', ...state, keys: selection.split(',') });
});
document.querySelector<HTMLSelectElement>('#pointer-action')!.addEventListener('change', () => { dragStart = null; });
controlFrame.addEventListener('click', event => {
  const state = inputState(); if (!state) return;
  const bounds = controlFrame.getBoundingClientRect();
  if (bounds.width <= 0 || bounds.height <= 0 || event.clientX < bounds.left || event.clientY < bounds.top || event.clientX >= bounds.right || event.clientY >= bounds.bottom) return;
  const x = Math.floor((event.clientX - bounds.left) * state.frame.width / bounds.width);
  const y = Math.floor((event.clientY - bounds.top) * state.frame.height / bounds.height);
  const mode = document.querySelector<HTMLSelectElement>('#pointer-action')!.value;
  if (mode === 'drag') {
    if (!dragStart) { dragStart = { x, y }; controlStatus.textContent = 'Drag start selected. Click the destination on the same fresh screenshot.'; return; }
    const start = dragStart; dragStart = null;
    void dispatchInput({ kind: 'drag', ...state, x: start.x, y: start.y, end_x: x, end_y: y, button: 'left' });
  } else if (mode === 'scroll-up' || mode === 'scroll-down') void dispatchInput({ kind: 'scroll', ...state, x, y, direction: mode === 'scroll-up' ? 'up' : 'down', ticks: 3 });
  else if (mode === 'click' || mode === 'right-click') void dispatchInput({ kind: 'click', ...state, x, y, button: mode === 'click' ? 'left' : 'right' });
});
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
  let frameCallback: number | undefined, interval: ReturnType<typeof setInterval> | undefined;
  let deadline: ReturnType<typeof setTimeout> | undefined;
  let receptionDeadline: ReceptionDeadline | undefined;
  let closePromise: Promise<void> | null = null;
  let mediaFailure: MediaFailure | null = null;
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
  video.hidden = false; lastFrame.hidden = true;
  const close = () => {
    if (closePromise) return closePromise;
    phase('cleanup-started');
    closed = true;
    pc.removeEventListener('icecandidate', candidate); pc.removeEventListener('icecandidateerror', candidateError);
    pc.removeEventListener('iceconnectionstatechange', iceStateChange); pc.removeEventListener('icegatheringstatechange', iceStateChange);
    endControlSession();
    clearInterval(interval); clearTimeout(deadline); receptionDeadline?.close();
    if (frameCallback !== undefined) video.cancelVideoFrameCallback(frameCallback);
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
        if (succeeded) { show({ status: 'stream-ended', error: message }); status.textContent = message; }
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
    if (succeeded) { show({ status: 'stream-ended', error: String(error) }); status.textContent = 'Stream ended; see result.'; }
    void beginCleanup().catch(() => {});
  };
  pc.onconnectionstatechange = () => { if (closed) return; phase('connection-state-change'); receptionDeadline?.update(); if (['failed', 'disconnected', 'closed'].includes(pc.connectionState)) fail(new CodecError('transport_failed', 'Native WebRTC transport failed.')); };
  pc.ontrack = event => {
    if (closed) return;
    phase('track-received');
    video.srcObject = new MediaStream([event.track]);
    void video.play().catch(error => fail(new CodecError('playback_failed', String(error))));
    const draw: VideoFrameRequestCallback = () => {
      if (closed) return;
      try {
        presentedFrames++; firstPresentedMs ??= performance.now() - started;
        if (presentedFrames === 1) phase('first-frame-presented');
        if (video.videoWidth > 0 && video.videoHeight > 0 && (lastFrame.width !== video.videoWidth || lastFrame.height !== video.videoHeight)) {
          lastFrame.width = video.videoWidth; lastFrame.height = video.videoHeight;
          lastFrame.style.aspectRatio = `${video.videoWidth} / ${video.videoHeight}`;
          video.style.aspectRatio = lastFrame.style.aspectRatio;
        }
        poster.drawImage(video, 0, 0, lastFrame.width, lastFrame.height);
        pixels.drawImage(video, 0, 0, 16, 9);
        let value = 2166136261;
        for (const byte of pixels.getImageData(0, 0, 16, 9).data) value = Math.imul(value ^ byte, 16777619) >>> 0;
        const hash = value.toString(16).padStart(8, '0'); if (!hashes.includes(hash) && hashes.length < 8) hashes.push(hash);
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
    phase('remote-description-installed');
    if (authenticationRequired) {
      authenticatedConfirmation = attemptAuthentication!.confirm(signal);
      await authenticatedConfirmation; phase('signed-confirmation-acknowledged');
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
      if (signal.aborted) { abort(); return; }
      receptionDeadline = new ReceptionDeadline({
        connected: () => pc.connectionState === 'connected',
        confirmed: () => !authenticationRequired || attemptAuthentication?.controlConfirmed === true,
        onPhase: phase,
        onTimeout: code => fail(new CodecError(code, code === 'connection_timeout'
          ? 'Native WebRTC connection did not establish within 12000ms after confirmation.'
          : 'Native stream did not produce a decoded/presented frame within 6000ms after connection and confirmation.')),
      });
      interval = setInterval(() => {
        if (checking || closed) return;
        checking = true;
        void (async () => {
          const stats = await pc.getStats();
          if (closed) return;
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
          const detail = { codec: { mimeType: codec.mimeType, payloadType: codec.payloadType, sdpFmtpLine: codec.sdpFmtpLine ?? null }, framesDecoded: inbound.framesDecoded, presentedFrames, distinctSampledPixelHashes: [...hashes], pixelChangeObserved: hashes.length >= 2, pixelSample: { width: 16, height: 9, algorithm: 'FNV1a32 RGBA (change detector only)' }, firstDecodedMs, firstPresentedMs, frameWidth: inbound.frameWidth, frameHeight: inbound.frameHeight, framesDropped: inbound.framesDropped ?? null, bytesReceived: inbound.bytesReceived, connectionState: pc.connectionState };
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
          // Successful session ownership transfers to the page, outside the attempt deadline.
          signal.removeEventListener('abort', abort);
          deadline = setTimeout(() => {
            show({ status: 'probe-deadline', endedAt: new Date().toISOString() }); status.textContent = '120-second diagnostic deadline reached; closing the session…';
            void beginCleanup().catch(() => {});
          }, 120000);
          resolve({ codec: codec.mimeType, framesDecoded: inbound.framesDecoded!, detail });
        })().catch(fail).finally(() => { checking = false; });
      }, 100);
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
  status.textContent = 'Negotiating native console…';
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
  } catch (error) {
    show({ status: 'failed', error: String(error), attempts: error instanceof NegotiationError ? error.attempts : [] }); status.textContent = error instanceof NegotiationError && error.code.startsWith('guest_') ? error.message : 'Native stream failed or cancelled.';
    controller = null; updateConnectionControls(); disconnect.disabled = true;
  }
});
disconnect.addEventListener('click', async () => {
  controller?.abort(); disconnect.disabled = true;
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
