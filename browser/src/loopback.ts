import { codecPreferences, CodecError, type AttemptResult, type CodecCapability, type VideoCodec } from './codec-policy.js';

export interface LoopbackOptions {
  track: MediaStreamTrack;
  video: HTMLVideoElement;
  lastFrame: HTMLCanvasElement;
  senderCodecs: readonly CodecCapability[];
  receiverCodecs: readonly CodecCapability[];
  // Explicit test injection, never a natural browser capability result.
  fault?: 'none' | 'h265-negotiation' | 'h265-first-frame';
}
interface CodecStats { mimeType: string; payloadType: number; sdpFmtpLine?: string }
export interface CodecSummary { payload: string; name: string; fmtp: string | null }
export function sdpCodecs(sdp: string | undefined): CodecSummary[] {
  const lines = (sdp ?? '').split(/\r?\n/);
  return lines.flatMap(line => {
    const match = /^a=rtpmap:(\d+) ([^\s]+)/.exec(line);
    if (!match?.[1] || !match[2]) return [];
    const prefix = `a=fmtp:${match[1]} `;
    return [{ payload: match[1], name: match[2], fmtp: lines.find(value => value.startsWith(prefix))?.slice(prefix.length) ?? null }];
  });
}
/** Actual browser-to-browser loopback; not the native host encoder or a VM media path. */
export async function probeLoopback(options: LoopbackOptions, preferred: VideoCodec, signal: AbortSignal): Promise<AttemptResult> {
  const started = performance.now();
  if (options.fault === 'h265-negotiation' && preferred === 'h265') throw new CodecError('injected_negotiation_failure', 'Explicit test fault before H.265 negotiation.');
  let sendPreferences: CodecCapability[];
  try { sendPreferences = codecPreferences(options.senderCodecs, preferred); }
  catch { throw new CodecError('loopback_sender_unavailable', `Browser cannot send ${preferred} in this loopback. This does not disprove receiving from a native host.`); }
  const receivePreferences = codecPreferences(options.receiverCodecs, preferred);
  const sender = new RTCPeerConnection({ iceServers: [] });
  const receiver = new RTCPeerConnection({ iceServers: [] });
  let interval: ReturnType<typeof setInterval> | undefined;
  let frameTimer: ReturnType<typeof setTimeout> | undefined;
  let frameCallback: number | undefined;
  let closed = false;
  let presentedFrames = 0;
  let firstPresentedMs: number | null = null;
  let firstDecodedObservedMs: number | null = null;
  let progressDeadlineStarted = false;
  const pixelHashes: string[] = [];
  const sample = document.createElement('canvas'); sample.width = 16; sample.height = 9;
  const pixels = sample.getContext('2d', { willReadFrequently: true })!;
  const last = options.lastFrame.getContext('2d')!;
  options.lastFrame.hidden = true; options.video.hidden = false;
  const close = () => {
    if (closed) return;
    closed = true;
    clearInterval(interval); clearTimeout(frameTimer);
    if (frameCallback !== undefined) options.video.cancelVideoFrameCallback(frameCallback);
    sender.close(); receiver.close();
    if (options.video.srcObject instanceof MediaStream) options.video.srcObject.getTracks().forEach(track => track.stop());
    options.video.srcObject = null;
    if (presentedFrames > 0) { options.video.hidden = true; options.lastFrame.hidden = false; }
  };
  signal.addEventListener('abort', close, { once: true });
  const queuedForSender: RTCIceCandidate[] = [], queuedForReceiver: RTCIceCandidate[] = [];
  let iceError: Error | null = null;
  async function addCandidate(peer: RTCPeerConnection, queue: RTCIceCandidate[], candidate: RTCIceCandidate | null) {
    if (!candidate || closed) return;
    if (!peer.remoteDescription) { queue.push(candidate); return; }
    try { await peer.addIceCandidate(candidate); } catch (error) { iceError = error instanceof Error ? error : new Error(String(error)); }
  }
  sender.onicecandidate = event => { void addCandidate(receiver, queuedForReceiver, event.candidate); };
  receiver.onicecandidate = event => { void addCandidate(sender, queuedForSender, event.candidate); };
  let playError: Error | null = null;
  receiver.ontrack = event => {
    options.video.srcObject = new MediaStream([event.track]);
    void options.video.play().catch(error => { playError = error instanceof Error ? error : new Error(String(error)); });
    if (typeof options.video.requestVideoFrameCallback === 'function') {
      const onFrame: VideoFrameRequestCallback = () => {
        if (closed) return;
        presentedFrames++;
        firstPresentedMs ??= performance.now() - started;
        last.drawImage(options.video, 0, 0, options.lastFrame.width, options.lastFrame.height);
        pixels.drawImage(options.video, 0, 0, sample.width, sample.height);
        // Small whole-frame fingerprint verifies change, not content integrity or image quality.
        let hash = 2166136261;
        for (const byte of pixels.getImageData(0, 0, sample.width, sample.height).data) hash = Math.imul(hash ^ byte, 16777619) >>> 0;
        const fingerprint = hash.toString(16).padStart(8, '0');
        if (!pixelHashes.includes(fingerprint) && pixelHashes.length < 8) pixelHashes.push(fingerprint);
        frameCallback = options.video.requestVideoFrameCallback(onFrame);
      };
      frameCallback = options.video.requestVideoFrameCallback(onFrame);
    }
  };
  try {
    signal.throwIfAborted();
    const send = sender.addTransceiver(options.track, { direction: 'sendonly' });
    send.setCodecPreferences(sendPreferences);
    await sender.setLocalDescription(await sender.createOffer());
    await receiver.setRemoteDescription(sender.localDescription!);
    for (const candidate of queuedForReceiver.splice(0)) await receiver.addIceCandidate(candidate);
    const receive = receiver.getTransceivers()[0];
    if (!receive) throw new CodecError('missing_transceiver', 'No video receiver after offer.');
    receive.direction = 'recvonly';
    receive.setCodecPreferences(receivePreferences);
    await receiver.setLocalDescription(await receiver.createAnswer());
    await sender.setRemoteDescription(receiver.localDescription!);
    for (const candidate of queuedForSender.splice(0)) await sender.addIceCandidate(candidate);
    signal.throwIfAborted();
    const negotiated = { offer: sdpCodecs(sender.localDescription?.sdp), answer: sdpCodecs(receiver.localDescription?.sdp) };
    const suppressFirstFrame = options.fault === 'h265-first-frame' && preferred === 'h265';
    return await new Promise<AttemptResult>((resolve, reject) => {
      let checking = false;
      const abort = () => reject(signal.reason ?? new CodecError('cancelled', 'Probe cancelled.'));
      signal.addEventListener('abort', abort, { once: true });
      const fail = (error: unknown) => { signal.removeEventListener('abort', abort); reject(error); };
      frameTimer = setTimeout(() => fail(new CodecError(suppressFirstFrame ? 'injected_first_frame_timeout' : 'first_frame_timeout', 'No verified first frame within 6000ms after answer.')), 6000);
      interval = setInterval(() => {
        if (checking || closed) return;
        checking = true;
        void (async () => {
          if (iceError) throw new CodecError('ice_failed', iceError.message);
          if (playError) throw new CodecError('playback_failed', playError.message);
          if (sender.connectionState === 'failed' || receiver.connectionState === 'failed') throw new CodecError('transport_failed', 'Loopback peer connection failed.');
          const stats = await receiver.getStats();
          let inbound: RTCInboundRtpStreamStats | undefined;
          stats.forEach(stat => { if (stat.type === 'inbound-rtp' && stat.kind === 'video' && stat.framesDecoded > 0) inbound = stat as RTCInboundRtpStreamStats; });
          if (!inbound?.codecId || suppressFirstFrame) return;
          const codec = stats.get(inbound.codecId) as CodecStats | undefined;
          if (!codec) return;
          // A decoder count alone is recorded separately from a presented browser frame.
          if (typeof options.video.requestVideoFrameCallback !== 'function') throw new CodecError('presentation_callback_unavailable', 'This dynamic probe requires requestVideoFrameCallback; codec policy itself does not.');
          if (presentedFrames < 1) return;
          firstDecodedObservedMs ??= performance.now() - started;
          if (!progressDeadlineStarted) {
            progressDeadlineStarted = true;
            clearTimeout(frameTimer);
            frameTimer = setTimeout(() => fail(new CodecError('frame_progress_timeout', 'First frame arrived but five dynamic presented/decoded frames were not verified within 3000ms.')), 3000);
          }
          if (inbound.framesDecoded! < 5 || presentedFrames < 5 || pixelHashes.length < 2) return;
          const outboundReport = await sender.getStats();
          const outbound: Record<string, unknown>[] = [];
          outboundReport.forEach(stat => { if (stat.type === 'outbound-rtp' && stat.kind === 'video') {
            const usedCodec = outboundReport.get(stat.codecId);
            outbound.push({ mimeType: usedCodec?.mimeType ?? null, framesEncoded: stat.framesEncoded ?? null, bytesSent: stat.bytesSent ?? null, encoderImplementation: stat.encoderImplementation ?? null, powerEfficientEncoder: stat.powerEfficientEncoder ?? null });
          } });
          signal.removeEventListener('abort', abort);
          resolve({ codec: codec.mimeType, framesDecoded: inbound.framesDecoded!, detail: {
            ...negotiated, firstDecodedObservedMs, firstPresentedMs, verificationCompletedMs: performance.now() - started, presentedFrames, distinctSampledPixelHashes: pixelHashes, pixelSample: { width: 16, height: 9, algorithm: 'FNV1a32 RGBA (change detector only)' },
            codec: { mimeType: codec.mimeType, payloadType: codec.payloadType, sdpFmtpLine: codec.sdpFmtpLine ?? null },
            inbound: { framesDecoded: inbound.framesDecoded, framesDropped: inbound.framesDropped ?? null, bytesReceived: inbound.bytesReceived, frameWidth: inbound.frameWidth, frameHeight: inbound.frameHeight },
            outbound, connectionState: receiver.connectionState, iceConnectionState: receiver.iceConnectionState,
            hostHardwareEncodingValidated: false,
          } });
        })().catch(fail).finally(() => { checking = false; });
      }, 100);
    });
  } finally { signal.removeEventListener('abort', close); close(); }
}
