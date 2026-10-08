import { AuthenticatedReceiver } from './native-auth.js';
import type { SignedGrant } from './session-auth.js';
import { codecName, nativeCodecPreferences, createNativeCodecPlan, negotiateCodec, CodecError, NegotiationError, type HostCapabilities, type AttemptResult, type VideoCodec } from './codec-policy.js';

const video = document.querySelector<HTMLVideoElement>('#received')!;
const lastFrame = document.querySelector<HTMLCanvasElement>('#last-frame')!;
const output = document.querySelector<HTMLPreElement>('#result')!;
const connect = document.querySelector<HTMLButtonElement>('#connect')!;
const disconnect = document.querySelector<HTMLButtonElement>('#disconnect')!;
const retryCleanup = document.querySelector<HTMLButtonElement>('#retry-cleanup')!;
const status = document.querySelector<HTMLElement>('#status')!;
let host: HostCapabilities | null = null;
let authenticationRequired = false;
let authVmId = "";
let authenticated: AuthenticatedReceiver | null = null;
let grantReady = false;
let controller: AbortController | null = null;
let closeCurrent: (() => Promise<void>) | null = null;
let cleanup: Promise<void> = Promise.resolve();
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
function waitForIce(pc: RTCPeerConnection, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const done = (error?: unknown) => { clearTimeout(timer); pc.removeEventListener('icegatheringstatechange', change); signal.removeEventListener('abort', abort); error ? reject(error) : resolve(); };
    const change = () => { if (pc.iceGatheringState === 'complete') done(); };
    const abort = () => done(signal.reason ?? new CodecError('cancelled', 'Cancelled.'));
    const timer = setTimeout(() => done(new CodecError('ice_gather_timeout', 'Host-only ICE gathering exceeded 3000ms.')), 3000);
    pc.addEventListener('icegatheringstatechange', change); signal.addEventListener('abort', abort, { once: true });
    if (signal.aborted) abort(); else change();
  });
}
async function nativeAttempt(preferred: VideoCodec, signal: AbortSignal): Promise<AttemptResult> {
  // Cleanup must finish before a fallback can reserve the server's single native worker.
  await cleanup;
  signal.throwIfAborted();
  const pc = new RTCPeerConnection({ iceServers: [] });
  const attemptAuthentication = authenticationRequired ? authenticated : null;
  const binding: { request_id: string; session_id?: string } = { request_id: crypto.randomUUID() };
  activeRequest = binding;
  let offered = false, closed = false, succeeded = false, checking = false;
  let authenticatedOffer: Promise<unknown> | null = null;
  let authenticatedConfirmation: Promise<void> | null = null;
  let frameCallback: number | undefined, interval: ReturnType<typeof setInterval> | undefined;
  let deadline: ReturnType<typeof setTimeout> | undefined;
  let closePromise: Promise<void> | null = null;
  let failReady: (error: unknown) => void = () => {};
  let presentedFrames = 0, firstPresentedMs: number | null = null, firstDecodedMs: number | null = null;
  const started = performance.now();
  const hashes: string[] = [];
  const sample = document.createElement('canvas'); sample.width = 16; sample.height = 9;
  const pixels = sample.getContext('2d', { willReadFrequently: true })!;
  const poster = lastFrame.getContext('2d')!;
  video.hidden = false; lastFrame.hidden = true;
  const close = () => {
    if (closePromise) return closePromise;
    closed = true;
    clearInterval(interval); clearTimeout(deadline);
    if (frameCallback !== undefined) video.cancelVideoFrameCallback(frameCallback);
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
          await authenticatedOffer;
          await authenticatedConfirmation?.catch(() => {});
          await attemptAuthentication.close(AbortSignal.timeout(5000));
        } else {
          const receipt = await post('/close', binding, AbortSignal.timeout(5000));
          if (receipt.closed !== true) throw new CodecError('cleanup_unconfirmed', 'Session cleanup was not acknowledged.');
        }
      }
      if (activeRequest === binding) activeRequest = null;
    })();
    return closePromise;
  };
  const beginCleanup = () => {
    const current = close(); cleanup = current; cleanupState = 'pending'; updateConnectionControls();
    void current.then(() => {
      if (cleanup !== current) return;
      cleanupState = 'idle'; updateConnectionControls();
    }, error => {
      if (cleanup !== current) return;
      cleanupState = 'failed'; updateConnectionControls();
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
    show({ attemptFailure: String(error) });
    failReady(error);
    if (succeeded) { show({ status: 'stream-ended', error: String(error) }); status.textContent = 'Stream ended; see result.'; }
    void beginCleanup().catch(() => {});
  };
  pc.onconnectionstatechange = () => { if (pc.connectionState === 'failed') fail(new CodecError('transport_failed', 'Native WebRTC transport failed.')); };
  pc.ontrack = event => {
    video.srcObject = new MediaStream([event.track]);
    void video.play().catch(error => fail(new CodecError('playback_failed', String(error))));
    const draw: VideoFrameRequestCallback = () => {
      if (closed) return;
      try {
        presentedFrames++; firstPresentedMs ??= performance.now() - started;
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
    const transceiver = pc.addTransceiver('video', { direction: 'recvonly' });
    transceiver.setCodecPreferences(nativeCodecPreferences(RTCRtpReceiver.getCapabilities('video')?.codecs ?? [], preferred));
    await pc.setLocalDescription(await pc.createOffer());
    await waitForIce(pc, signal);
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
    binding.session_id = answer.session_id;
    signal.throwIfAborted();
    await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
    if (authenticationRequired) {
      authenticatedConfirmation = attemptAuthentication!.confirm(signal);
      await authenticatedConfirmation;
    }
    const result = await new Promise<AttemptResult>((resolve, reject) => {
      failReady = reject;
      if (signal.aborted) { abort(); return; }
      deadline = setTimeout(() => fail(new CodecError('first_frame_timeout', 'Native stream did not produce a decoded/presented frame within 6000ms after the answer.')), 6000);
      interval = setInterval(() => {
        if (checking || closed) return;
        checking = true;
        void (async () => {
          const stats = await pc.getStats();
          if (closed) return;
          let inbound: RTCInboundRtpStreamStats | undefined;
          stats.forEach(stat => { if (stat.type === 'inbound-rtp' && stat.kind === 'video' && stat.framesDecoded > 0) inbound = stat as RTCInboundRtpStreamStats; });
          if (!inbound?.codecId || presentedFrames < 1) return;
          const codec = stats.get(inbound.codecId);
          if (!codec) return;
          if (firstDecodedMs === null) {
            firstDecodedMs = performance.now() - started;
            clearTimeout(deadline);
            deadline = setTimeout(() => fail(new CodecError('frame_progress_timeout', 'No five decoded/presented native frames within 6000ms after first frame.')), 6000);
          }
          const detail = { codec: { mimeType: codec.mimeType, payloadType: codec.payloadType, sdpFmtpLine: codec.sdpFmtpLine ?? null }, framesDecoded: inbound.framesDecoded, presentedFrames, distinctSampledPixelHashes: [...hashes], pixelChangeObserved: hashes.length >= 2, pixelSample: { width: 16, height: 9, algorithm: 'FNV1a32 RGBA (change detector only)' }, firstDecodedMs, firstPresentedMs, frameWidth: inbound.frameWidth, frameHeight: inbound.frameHeight, framesDropped: inbound.framesDropped ?? null, bytesReceived: inbound.bytesReceived, connectionState: pc.connectionState };
          const selected = codecName(String(codec.mimeType));
          if (!selected || (preferred === 'h264' && selected !== 'h264') || host?.[selected] !== 'verified-hardware') throw new CodecError('unexpected_codec', 'Native sender used a codec outside the verified hardware policy.');
          nativeCodecPreferences([{ mimeType: codec.mimeType, clockRate: 90000, sdpFmtpLine: codec.sdpFmtpLine ?? '' }, ...(selected === 'h265' ? nativeCodecPreferences(RTCRtpReceiver.getCapabilities('video')?.codecs ?? [], 'h264') : [])], selected);
          show({ latestStats: detail });
          if (succeeded) return;
          if (inbound.framesDecoded! < 5 || presentedFrames < 5) return;
          succeeded = true;
          clearTimeout(deadline);
          // Successful session ownership transfers to the page, outside the attempt deadline.
          signal.removeEventListener('abort', abort);
          deadline = setTimeout(() => {
            show({ status: 'probe-deadline', endedAt: new Date().toISOString() }); status.textContent = '120-second diagnostic deadline reached; stream closed.';
            void beginCleanup().catch(() => {});
          }, 120000);
          resolve({ codec: codec.mimeType, framesDecoded: inbound.framesDecoded!, detail });
        })().catch(fail).finally(() => { checking = false; });
      }, 100);
    });
    return result;
  } finally {
    signal.removeEventListener('abort', abort);
    if (!succeeded) await beginCleanup();
  }
}

async function loadCapabilities() {
  const response = await fetch('/capabilities', { cache: 'no-store', credentials: 'omit', signal: AbortSignal.timeout(8000) });
  if (!response.ok) throw new Error(`Native capability probe returned HTTP ${response.status}.`);
  const value = await response.json() as Record<string, unknown>;
  const allowed = ['verified-hardware', 'unavailable', 'unverified'];
  if (!allowed.includes(String(value.h264)) || !allowed.includes(String(value.h265))) throw new Error('Native hardware capability schema invalid.');
  host = { h264: value.h264 as HostCapabilities['h264'], h265: value.h265 as HostCapabilities['h265'] };
  show({ status: 'ready', hostCapabilityReport: value, receiverCapabilities: RTCRtpReceiver.getCapabilities('video'), scope: 'localhost-native-vm-webrtc', internetRemoteValidated: false });
  const auth = value.authentication as { required?: boolean; vm_id?: string } | undefined;
  authenticationRequired = auth?.required === true;
  if (authenticationRequired) {
    if (typeof auth?.vm_id !== 'string') throw new Error('Authenticated service VM binding missing.');
    authVmId = auth.vm_id; document.querySelector<HTMLElement>('#pairing')!.hidden = false;
    status.textContent = 'Local signed session requires a verified pairing grant.'; connect.disabled = true;
  } else { status.textContent = 'Native hardware probe loaded. Unsigned localhost diagnostic ready.'; connect.disabled = false; }
}
void loadCapabilities().catch(error => { show({ status: 'blocked', error: String(error) }); status.textContent = 'Native service unavailable or incompatible; no fixture capability substituted.'; });
connect.addEventListener('click', async () => {
  if (controller || !host || cleanupState !== 'idle' || (authenticationRequired && !grantReady)) return;
  controller = new AbortController(); connect.disabled = true; disconnect.disabled = false;
  status.textContent = 'Negotiating native console…';
  report = { hostCapabilityReport: report.hostCapabilityReport, receiverCapabilities: report.receiverCapabilities, scope: 'localhost-native-vm-webrtc', internetRemoteValidated: false };
  show({ status: 'connecting', startedAt: new Date().toISOString(), userAgent: navigator.userAgent });
  try {
    const plan = createNativeCodecPlan(host, RTCRtpReceiver.getCapabilities('video')?.codecs ?? []);
    const preference = document.querySelector<HTMLSelectElement>('#codec-preference')!.value;
    if (preference === 'h264') { plan.preferred = 'h264'; plan.reason = 'explicit_h264_test'; }
    show({ requestedPreference: preference });
    const result = await negotiateCodec(plan, nativeAttempt, controller.signal);
    show({ status: 'passed-live', policyReason: plan.reason, ...result, verifiedAt: new Date().toISOString() });
    status.textContent = `Receiving native ${result.selected.toUpperCase()} (${result.fallbackReason ?? 'preferred codec'}). Disconnect ends this session.`;
  } catch (error) {
    show({ status: 'failed', error: String(error), attempts: error instanceof NegotiationError ? error.attempts : [] }); status.textContent = 'Native stream failed or cancelled.';
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
    authenticated = await AuthenticatedReceiver.open({ pinnedHostPublicKey: document.querySelector<HTMLInputElement>('#host-pin')!.value.trim(), vmId: authVmId, databaseName: 'veynode-session-auth-native-test' });
    const request = await authenticated.pairingRequest();
    document.querySelector<HTMLElement>('#pairing-request')!.textContent = JSON.stringify(request, null, 2);
    document.querySelector<HTMLButtonElement>('#accept-grant')!.disabled = false;
    pairingStatus.textContent = 'Compare this digest locally before approving the observe grant.';
  })().catch(error => {pairingStatus.textContent = String(error);});
});
document.querySelector<HTMLButtonElement>('#accept-grant')!.addEventListener('click', () => {
  if (controller || cleanupState !== 'idle' || !authenticated) return;
  void (async () => {
    await authenticated!.acceptGrant(JSON.parse(document.querySelector<HTMLTextAreaElement>('#signed-grant')!.value) as SignedGrant);
    grantReady = true; updateConnectionControls();
    pairingStatus.textContent = 'Host signature and device/VM/observe bindings verified. Ready.';
  })().catch(error => {grantReady = false; connect.disabled = true; pairingStatus.textContent = String(error);});
});
