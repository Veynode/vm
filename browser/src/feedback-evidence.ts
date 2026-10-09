// Diagnostic metadata only: no SDP, credentials, addresses or unknown attributes
// leave this function. Never use this summary to authorize or drive media.
const feedbackKinds = ['nack', 'nack pli', 'ccm fir', 'transport-cc', 'goog-remb'];
const extensionKinds = new Map([
  ['http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01', 'transport-wide-sequence'],
  ['urn:ietf:params:rtp-hdrext:sdes:mid', 'mid'],
  ['http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time', 'absolute-send-time'],
  ['urn:ietf:params:rtp-hdrext:toffset', 'timestamp-offset'],
]);

export function videoFeedbackSummary(sdp: string | undefined, payloadType: unknown) {
  const unavailable = { status: 'unavailable' as const };
  if (typeof sdp !== 'string' || sdp.length > 262144 || !Number.isInteger(payloadType)
      || (payloadType as number) < 0 || (payloadType as number) > 127) return unavailable;
  const lines = sdp.split(/\r?\n/);
  if (lines.length > 4096 || lines.some(line => line.length > 2048)) return unavailable;
  const matches: string[][] = [];
  let section: string[] | undefined;
  for (const line of lines) {
    if (line.startsWith('m=')) {
      section = undefined;
      const fields = line.split(/\s+/);
      if (fields[0] === 'm=video' && /^[1-9][0-9]{0,4}$/.test(fields[1] ?? '')
          && Number(fields[1]) <= 65535 && fields.slice(3).includes(String(payloadType))) {
        section = []; matches.push(section);
      }
    } else section?.push(line);
  }
  if (matches.length !== 1) return { status: matches.length ? 'ambiguous' as const : 'unavailable' as const };
  const feedback = new Set<string>();
  const extensions = new Set<string>();
  let unknownFeedbackCount = 0, unknownExtensionCount = 0;
  for (const line of matches[0]!) {
    const fb = /^a=rtcp-fb:(\*|[0-9]{1,3})\s+(.+)$/.exec(line);
    if (fb && (fb[1] === '*' || Number(fb[1]) === payloadType)) {
      if (feedbackKinds.includes(fb[2]!)) feedback.add(fb[2]!);
      else unknownFeedbackCount++;
    }
    const ext = /^a=extmap:([0-9]{1,3})(?:\/(sendonly|recvonly|sendrecv|inactive))?\s+(\S+)(?:\s+.*)?$/.exec(line);
    if (ext) {
      const kind = extensionKinds.get(ext[3]!);
      if (kind && Number(ext[1]) >= 1 && Number(ext[1]) <= 255) extensions.add(`${kind}:${ext[2] ?? 'sendrecv'}`);
      else unknownExtensionCount++;
    }
  }
  return { status: 'observed' as const, payloadType: payloadType as number,
    feedback: [...feedback].sort(), extensions: [...extensions].sort(),
    unknownFeedbackCount, unknownExtensionCount,
    scope: 'selected video SDP attributes; not evidence of packets or bandwidth estimation' };
}
