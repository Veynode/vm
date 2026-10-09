import test from 'node:test';
import assert from 'node:assert/strict';
import { videoFeedbackSummary } from '../dist/feedback-evidence.js';

test('selects only live video payload and safe feedback metadata', () => {
  const result = videoFeedbackSummary([
    'v=0', 'a=ice-pwd:private-secret', 'm=audio 9 UDP/TLS/RTP/SAVPF 111',
    'a=rtcp-fb:111 goog-remb', 'm=video 9 UDP/TLS/RTP/SAVPF 96 97',
    'a=rtcp-fb:96 nack', 'a=rtcp-fb:96 nack pli', 'a=rtcp-fb:* transport-cc',
    'a=rtcp-fb:97 goog-remb', 'a=rtcp-fb:96 nack',
    'a=extmap:4/sendonly http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01',
    'a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid',
    'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'a=rtcp-fb:* ccm fir',
  ].join('\r\n'), 96);
  assert.equal(result.status, 'observed');
  assert.deepEqual(result.feedback, ['nack', 'nack pli', 'transport-cc']);
  assert.deepEqual(result.extensions, ['mid:sendrecv', 'transport-wide-sequence:sendonly']);
  assert.equal(JSON.stringify(result).includes('private-secret'), false);
});

test('unknown attributes and extension suffixes never escape into diagnostics', () => {
  const secret = 'DO-NOT-EXPORT';
  const result = videoFeedbackSummary(`m=video 9 UDP/TLS/RTP/SAVPF 96\n`
    + `a=rtcp-fb:96 ${secret}\na=extmap:2 https://private.invalid/${secret}\n`
    + `a=extmap:3/sendonly urn:ietf:params:rtp-hdrext:sdes:mid ${secret}\n`
    + `a=candidate:${secret}\na=ice-ufrag:${secret}`, 96);
  assert.equal(result.status, 'observed');
  assert.equal(result.unknownFeedbackCount, 1);
  assert.equal(result.unknownExtensionCount, 1);
  assert.equal(JSON.stringify(result).includes(secret), false);
});

test('absent, rejected, oversized and ambiguous sections do not invent negotiation', () => {
  for (const [sdp, pt] of [[undefined, 96], ['', 96], ['x'.repeat(262145), 96],
    ['m=video 0 UDP/TLS/RTP/SAVPF 96', 96], ['m=video 9 UDP/TLS/RTP/SAVPF 97', 96],
    ['m=video 9 UDP/TLS/RTP/SAVPF 96', '96'], ['m=video 9 UDP/TLS/RTP/SAVPF 96', NaN]]) {
    assert.equal(videoFeedbackSummary(sdp, pt).status, 'unavailable');
  }
  assert.equal(videoFeedbackSummary('m=video 9 UDP/TLS/RTP/SAVPF 96\nm=video 9 UDP/TLS/RTP/SAVPF 96', 96).status, 'ambiguous');
});
