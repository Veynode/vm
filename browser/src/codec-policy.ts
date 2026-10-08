/** Receiver-side policy. Hardware evidence must come from the trusted host encoder. */
export type VideoCodec = 'h264' | 'h265';
export type HardwareStatus = 'verified-hardware' | 'unavailable' | 'unverified';
export interface HostCapabilities { h264: HardwareStatus; h265: HardwareStatus }
export interface CodecCapability { mimeType: string; clockRate: number; channels?: number; sdpFmtpLine?: string }
export interface CodecPlan {
  preferred: VideoCodec;
  reason: string;
  receiverCodecs: readonly CodecCapability[];
  host: Readonly<HostCapabilities>;
}
export interface AttemptResult { codec: string; framesDecoded: number; detail?: unknown }
export interface AttemptRecord { preferred: VideoCodec; outcome: 'success' | 'failed'; reason: string }
export interface PolicyResult { selected: VideoCodec; fallbackReason: string | null; attempts: AttemptRecord[]; result: AttemptResult }
export class CodecError extends Error {
  constructor(public readonly code: string, message: string) { super(message); this.name = 'CodecError'; }
}
export class NegotiationError extends CodecError {
  constructor(code: string, message: string, public readonly attempts: AttemptRecord[]) { super(code, message); }
}
export function codecName(mime: string): VideoCodec | null {
  switch (mime.toLowerCase()) { case 'video/h264': return 'h264'; case 'video/h265': return 'h265'; default: return null; }
}
export function supports(codecs: readonly CodecCapability[], codec: VideoCodec): boolean {
  return codecs.some(c => codecName(c.mimeType) === codec);
}
export function createCodecPlan(host: HostCapabilities, receiverCodecs: readonly CodecCapability[]): CodecPlan {
  if (host.h264 !== 'verified-hardware') throw new CodecError('hardware_h264_required', 'A verified hardware H.264 fallback is required. Software host encoding is not permitted.');
  if (!supports(receiverCodecs, 'h264')) throw new CodecError('browser_h264_unavailable', 'This browser does not advertise WebRTC H.264 receiving.');
  const preferred = host.h265 === 'verified-hardware' && supports(receiverCodecs, 'h265') ? 'h265' : 'h264';
  const reason = preferred === 'h265' ? 'host_and_receiver_h265_available' : host.h265 !== 'verified-hardware' ? 'host_h265_hardware_unverified_or_unavailable' : 'browser_h265_unavailable';
  return { preferred, reason, receiverCodecs: [...receiverCodecs], host: { ...host } };
}
/** Preserve original capability objects/profile variants. Never return []: it restores browser defaults. */
export function codecPreferences(codecs: readonly CodecCapability[], preferred: VideoCodec): CodecCapability[] {
  if (!supports(codecs, preferred)) throw new CodecError('codec_unavailable', `WebRTC ${preferred} capability unavailable.`);
  const order: VideoCodec[] = preferred === 'h265' ? ['h265', 'h264'] : ['h264'];
  const primary = order.flatMap(name => codecs.filter(codec => codecName(codec.mimeType) === name));
  const auxiliary = codecs.filter(codec => /^(video\/)(rtx|red|ulpfec|flexfec-03)$/i.test(codec.mimeType));
  return [...primary, ...auxiliary];
}
export type CodecAttempt = (preferred: VideoCodec, signal: AbortSignal) => Promise<AttemptResult>;
/** Each attempt owns and closes its transports on abort/return. At most one H.265 attempt, one H.264 attempt. */
export async function negotiateCodec(plan: CodecPlan, attempt: CodecAttempt, signal?: AbortSignal, attemptTimeoutMs = 12000): Promise<PolicyResult> {
  if (!Number.isInteger(attemptTimeoutMs) || attemptTimeoutMs < 1 || attemptTimeoutMs > 30000) throw new CodecError('invalid_timeout', 'Attempt deadline must be 1..30000ms.');
  const attempts: AttemptRecord[] = [];
  const choices: VideoCodec[] = plan.preferred === 'h265' ? ['h265', 'h264'] : ['h264'];
  let fallbackReason: string | null = plan.preferred === 'h264' ? plan.reason : null;
  for (const preferred of choices) {
    if (signal?.aborted) throw new NegotiationError('cancelled', 'Negotiation cancelled.', attempts);
    const controller = new AbortController();
    const cancel = () => controller.abort(new CodecError('cancelled', 'Negotiation cancelled.'));
    signal?.addEventListener('abort', cancel, { once: true });
    let rejectAbort: (reason: unknown) => void = () => {};
    const aborted = new Promise<never>((_, reject) => { rejectAbort = reject; });
    const onAbort = () => rejectAbort(controller.signal.reason);
    controller.signal.addEventListener('abort', onAbort, { once: true });
    const timer = setTimeout(() => controller.abort(new CodecError('attempt_timeout', 'Codec attempt exceeded its deadline.')), attemptTimeoutMs);
    try {
      const result = await Promise.race([attempt(preferred, controller.signal), aborted]);
      const codec = codecName(result.codec);
      if (!codec || (preferred === 'h264' && codec !== 'h264') || plan.host[codec] !== 'verified-hardware') throw new CodecError('unexpected_codec', `Unapproved codec ${result.codec}; no software fallback allowed.`);
      if (!Number.isFinite(result.framesDecoded) || result.framesDecoded < 1) throw new CodecError('no_decoded_frame', 'No actual decoded RTP frame was verified.');
      const reason = preferred === 'h265' && codec === 'h264' ? 'peer_selected_h264' : 'decoded_frame_verified';
      attempts.push({ preferred, outcome: 'success', reason });
      if (reason === 'peer_selected_h264') fallbackReason = reason;
      return { selected: codec, fallbackReason, attempts, result };
    } catch (error) {
      const reason = error instanceof CodecError ? error.code : 'negotiation_failed';
      attempts.push({ preferred, outcome: 'failed', reason });
      if (signal?.aborted || reason === 'cancelled') throw new NegotiationError('cancelled', 'Negotiation cancelled.', attempts);
      fallbackReason = reason;
      if (preferred === 'h264') throw new NegotiationError('no_working_hardware_codec', error instanceof Error ? error.message : String(error), attempts);
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener('abort', cancel);
      controller.signal.removeEventListener('abort', onAbort);
      controller.abort();
    }
  }
  throw new NegotiationError('no_working_hardware_codec', 'No permitted codec succeeded.', attempts);
}

function fmtpParameters(line: string | undefined): Map<string, string> | null {
  const values = new Map<string, string>();
  for (const part of (line ?? '').split(';')) {
    if (!part.trim()) continue;
    const equals = part.indexOf('=');
    if (equals < 1) return null;
    const key = part.slice(0, equals).trim().toLowerCase();
    const value = part.slice(equals + 1).trim();
    if (!key || !value || values.has(key)) return null;
    values.set(key, value);
  }
  return values;
}
const H264_NATIVE_LEVELS = new Set([31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62]);
const H265_NATIVE_LEVELS = new Set([120, 123, 150, 153, 156, 180, 183, 186]);
/** Capability filter for this native encoder, not a generic browser codec claim. */
function nativeCompatible(codec: CodecCapability): boolean {
  if (codec.clockRate !== 90000) return false;
  const params = fmtpParameters(codec.sdpFmtpLine);
  if (!params) return false;
  if (codecName(codec.mimeType) === 'h264') {
    if (params.get('packetization-mode') !== '1') return false;
    const profile = params.get('profile-level-id');
    // Missing profile must not be assumed to advertise our CB level 3.1 path.
    if (!profile || !/^[0-9a-f]{6}$/i.test(profile)) return false;
    const idc = parseInt(profile.slice(0, 2), 16);
    const iop = parseInt(profile.slice(2, 4), 16);
    const level = parseInt(profile.slice(4, 6), 16);
    // RFC 6184 table 5: 42 x1xx0000, 4d 1xxx0000, 58 11xx0000.
    const constrainedBaseline = (iop & 0x0f) === 0 && ((idc === 0x42 && (iop & 0x40) !== 0) || (idc === 0x4d && (iop & 0x80) !== 0) || (idc === 0x58 && (iop & 0xc0) === 0xc0));
    return constrainedBaseline && H264_NATIVE_LEVELS.has(level);
  }
  if (codecName(codec.mimeType) === 'h265') {
    // RFC 7798 defaults: profile-space=0, profile-id=1, tier=0, tx-mode=SRST.
    // No default level satisfies the native encoder's required level >= 4.
    const level = params.get('level-id');
    return (params.get('profile-space') ?? '0') === '0'
      && (params.get('profile-id') ?? '1') === '1'
      && (params.get('tier-flag') ?? '0') === '0'
      && (params.get('tx-mode') ?? 'SRST') === 'SRST'
      && level !== undefined && /^\d{1,3}$/.test(level) && H265_NATIVE_LEVELS.has(Number(level));
  }
  return false;
}
/** CB level >=3.1 mode1 plus HEVC Main tier0/SRST level >=4, original objects only. */
export function nativeCodecPreferences(codecs: readonly CodecCapability[], preferred: VideoCodec): CodecCapability[] {
  const filtered = codecs.filter(codec => codecName(codec.mimeType) === null || nativeCompatible(codec));
  if (!supports(filtered, 'h264')) throw new CodecError('unsupported_profile', 'Native H.264 requires explicit Constrained Baseline level >=3.1, packetization-mode=1 receiving support.');
  if (preferred === 'h265' && !supports(filtered, 'h265')) throw new CodecError('unsupported_profile', 'Native H.265 requires Main profile, tier 0, SRST and level >=4 receiving support.');
  return codecPreferences(filtered, preferred);
}
export function createNativeCodecPlan(host: HostCapabilities, receiverCodecs: readonly CodecCapability[]): CodecPlan {
  const h264 = nativeCodecPreferences(receiverCodecs, 'h264');
  let compatible = h264;
  try { compatible = nativeCodecPreferences(receiverCodecs, 'h265'); }
  catch (error) { if (!(error instanceof CodecError) || error.code !== 'unsupported_profile') throw error; }
  const plan = createCodecPlan(host, compatible);
  if (host.h265 === 'verified-hardware' && plan.preferred === 'h264' && supports(receiverCodecs, 'h265')) plan.reason = 'browser_h265_profile_unavailable';
  return plan;
}
