import { createCodecPlan, negotiateCodec, NegotiationError, type HostCapabilities } from './codec-policy.js';
import { probeLoopback } from './loopback.js';

const video = document.querySelector<HTMLVideoElement>('#received')!;
const canvas = document.querySelector<HTMLCanvasElement>('#source')!;
const output = document.querySelector<HTMLPreElement>('#result')!;
const run = document.querySelector<HTMLButtonElement>('#run')!;
const stop = document.querySelector<HTMLButtonElement>('#stop')!;
const status = document.querySelector<HTMLElement>('#status')!;
let active: AbortController | null = null;
let lastReport: unknown = null;
function show(report: unknown) { lastReport = report; output.textContent = JSON.stringify(report, null, 2); }
function capabilities() {
  if (typeof RTCRtpReceiver === 'undefined' || typeof RTCRtpSender === 'undefined') throw new Error('WebRTC RTP capabilities are unavailable.');
  return { receiver: RTCRtpReceiver.getCapabilities('video')?.codecs ?? [], sender: RTCRtpSender.getCapabilities('video')?.codecs ?? [] };
}
try { show({ status: 'ready', capabilities: capabilities(), notice: 'Real WebRTC loopback probe. No VM or native host encoder is connected.' }); }
catch (error) { show({ status: 'unsupported', error: String(error) }); run.disabled = true; }
run.addEventListener('click', async () => {
  if (active) return;
  active = new AbortController();
  run.disabled = true; stop.disabled = false;
  status.textContent = 'Running real loopback negotiation…';
  const startedAt = new Date().toISOString();
  const caps = capabilities();
  const hardwareFixture: HostCapabilities = {
    h264: document.querySelector<HTMLInputElement>('#h264')!.checked ? 'verified-hardware' : 'unverified',
    h265: document.querySelector<HTMLInputElement>('#h265')!.checked ? 'verified-hardware' : 'unverified',
  };
  const fault = document.querySelector<HTMLSelectElement>('#fault')!.value as 'none' | 'h265-negotiation' | 'h265-first-frame';
  const context = canvas.getContext('2d')!;
  let frame = 0;
  const animate = () => {
    context.fillStyle = `hsl(${frame % 360} 70% 30%)`; context.fillRect(0, 0, 640, 360);
    context.fillStyle = '#ffffff'; context.font = '32px monospace'; context.fillText(`WebRTC frame ${frame++}`, 30, 80);
    context.fillRect((frame * 4) % 600, 150, 40, 140);
  };
  animate(); const animation = setInterval(animate, 1000 / 30);
  const stream = canvas.captureStream(30);
  const base = { startedAt, userAgent: navigator.userAgent, scope: 'browser-canvas-webrtc-loopback', vmMediaValidated: false, nativeHostEncoderValidated: false, hostCapabilityInput: { source: 'explicit_probe_fixture', ...hardwareFixture }, faultInjection: fault, capabilities: caps };
  try {
    const plan = createCodecPlan(hardwareFixture, caps.receiver);
    const result = await negotiateCodec(plan, (preferred, signal) => probeLoopback({ track: stream.getVideoTracks()[0]!, video, lastFrame: document.querySelector<HTMLCanvasElement>('#last-frame')!, senderCodecs: caps.sender, receiverCodecs: caps.receiver, fault }, preferred, signal), active.signal);
    show({ ...base, status: 'passed', policyReason: plan.reason, ...result, completedAt: new Date().toISOString() });
    status.textContent = `Decoded ${result.selected.toUpperCase()} over WebRTC. ${result.fallbackReason ? `Fallback: ${result.fallbackReason}.` : 'Preferred codec verified.'} Probe closed.`;
  } catch (error) {
    show({ ...base, status: 'failed', error: error instanceof Error ? error.message : String(error), attempts: error instanceof NegotiationError ? error.attempts : [], completedAt: new Date().toISOString() });
    status.textContent = 'Probe failed or cancelled; see exact result below.';
  } finally {
    clearInterval(animation); stream.getTracks().forEach(track => track.stop());
    active = null; run.disabled = false; stop.disabled = true;
  }
});
stop.addEventListener('click', () => active?.abort());
window.addEventListener('pagehide', () => active?.abort());
document.querySelector<HTMLButtonElement>('#download')!.addEventListener('click', () => {
  const url = URL.createObjectURL(new Blob([JSON.stringify(lastReport, null, 2)], { type: 'application/json' }));
  const link = document.createElement('a'); link.href = url; link.download = `webrtc-codec-${Date.now()}.json`; link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});
