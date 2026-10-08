# Browser WebRTC codec policy and local protocol probe

This package implements receiver codec selection and bounded H.265 → H.264 fallback. Its native receiver is connected to the actual VM capture/encoder diagnostic; production control-service integration remains unfinished. H.264 remains the initial stable baseline. H.265 is preferred only when the trusted host reports **verified hardware encoding** and the browser advertises WebRTC receiving support. A required hardware H.264 fallback prevents silently substituting host software encoding.

## Reproduce

```sh
cd browser
npm ci --ignore-scripts
npm test
npm run serve
```

Open `http://127.0.0.1:8765/` in the actual target browser without experimental flags. The server binds only loopback, serves a fixed asset allowlist, rejects unexpected Host headers and permits no writes. Set `PORT=0 npm run serve` for an allocated port printed in JSON. No external ICE/STUN/TURN, capture permissions or cloud services are used.

1. Leave both explicitly labelled host capability **fixtures** selected, no fault injection, and run the probe. Those checkboxes do not measure the host.
2. Read/download the JSON result. `capabilities.receiver` is only an advertisement. A passed run also has real offer/answer codec profiles, inbound RTP `codecId` resolved to MIME, at least five `framesDecoded` and five presented frames, plus at least two distinct downsampled pixel fingerprints from the actual received video. This dynamic probe requires `requestVideoFrameCallback`; the reusable first-frame policy does not. A final received frame remains visible after closing the transport. The actual selected codec may be H.264 even when H.265 appears first in SDP.
3. On a browser with H.265 receive/send support, select the negotiation fault and run again: one failed H.265 attempt must lead to a real H.264 peer connection. Repeat with the first-frame fault: actual H.265 frame evidence is deliberately suppressed until its six-second timeout, then H.264 must decode. These injected failures are labelled in every report; they do not prove a natural browser defect.
4. Deselect H.265 host capability and verify a single H.264 attempt. Deselect H.264 and verify a fail-closed result without a peer connection. Cancel during an attempt and verify that no fallback starts.

Each attempt has a 12-second total deadline, with a six-second first-frame deadline after the answer and a three-second progress deadline to verify five dynamic frames once the first frame arrives (still within the total deadline). The policy tries at most two connections, closes the previous connection before fallback, and aborts without retry on cancellation. Canvas capture/animation and all peer connections stop on success, failure, cancellation, and page exit. This is a short codec acceptance probe, not sustained streaming or a latency benchmark.

## Integration contract

`src/codec-policy.ts` is independent of the UI. `createCodecPlan(host, receiverCodecs)` takes the native host encoder report and `RTCRtpReceiver.getCapabilities('video').codecs`. Production callers must obtain the hardware report from an authenticated trusted host, for the actual requested codec configuration, and keep the native encoder configured to **require hardware acceleration**. A browser checkbox, decoder statistic, SDP entry, HTML video file playback, or codec name cannot authenticate that report or prove host hardware use. If that host capability changes or its hardware session fails, terminate the attempt and run the permitted H.264 fallback; never silently open a software session.

`codecPreferences()` returns original advertised codec objects, keeping profile variants and RTX/RED/FEC while excluding unapproved primary video codecs. It never returns an empty array (which would reset preferences to browser defaults). Apply it to the browser receiving transceiver before the answer. H.265 preferences retain H.264; the fallback attempt permits only H.264 primary media. The native sender must match RTP payload/profile/packetization from the negotiated SDP. Actual profile compatibility is determined through offer/answer and real decoded frames, not just MIME availability.

`negotiateCodec(plan, attempt, signal)` runs an application-supplied transport attempt. Its adapter must synchronously close transport resources when its supplied AbortSignal fires and must not let old asynchronous negotiation work create new connections after abort. Return actual inbound codec MIME and a positive decoded-frame count. Failed negotiation, hardware-session failure or first-frame timeout invokes H.264 once. An unapproved primary codec, no decoded frames, or two failures is an explicit error. No automatic retry loop is allowed. A successful preferred offer that actually selects H.264 is reported as `peer_selected_h264`.

`src/loopback.ts` is the real-browser test adapter, not the production sender. It additionally needs this browser to **send** the codec from canvas. `loopback_sender_unavailable` limits this test; it does not disprove receiving that codec from a native host. Browser loopback encoding may use software. Reports always include `nativeHostEncoderValidated:false` and `vmMediaValidated:false`; any optional browser encoder implementation statistic applies solely to that browser loopback. Native HEVC/H.264 encoding, RTP packetization from it, remote Internet connectivity, paired endpoint trust, TURN, adaptive rate control and VM input are separate integration gates.

Evidence records only codec summaries, not full SDP/ICE credentials or candidate addresses. The local page has no evidence-upload endpoint.

## Validation and sources

`npm test` builds strict TypeScript and runs focused unit fixtures for codec ordering/profile preservation, missing hardware/browser capability, successful actual-codec checks, bounded fallback, deadline/cancel behavior, disallowed media, and credential-free SDP summaries. Unit fixtures do not certify a browser or VM. Real browser result JSON must be saved separately with the browser version; no browser-specific success is claimed by these unit tests.

The [W3C WebRTC specification](https://www.w3.org/TR/webrtc/) defines receiver capability queries and `setCodecPreferences`; advertised capability is optimistic and preferences affect subsequent offer/answer. The [W3C statistics specification](https://www.w3.org/TR/webrtc-stats/) defines the inbound RTP codec reference and decoded-frame statistics used here. Chrome announced [WebRTC HEVC in Chrome 136](https://developer.chrome.com/blog/chrome-136-beta), but platform-dependent availability is queried at runtime rather than inferred from version or user agent. No experimental flags are requested.

## Native host receiver probe

`native.html` / `src/native.ts` is a separate receiver for the real local native sender. It is served by the native WebRTC service, **not** `serve.mjs`. The service must serve `/` → `native.html`, `/dist/native.js`, `/dist/codec-policy.js`, and `/probe.css` from this package. Build first with `npm run build`.

The same-origin local API contract is:

- `GET /capabilities` returns top-level `h264` and `h265` statuses (`verified-hardware`, `unavailable`, `unverified`), plus optional evidence fields. These must reflect actual native encoder probing. There are no capability fixture controls on this page. The explicit H.264-only preference changes codec selection only; it never changes or substitutes the native hardware report.
- `POST /offer` accepts `{request_id: UUIDv4, type: "offer", sdp, codec: "h264"|"h265"}` and returns `{session_id, type: "answer", sdp}`. The browser is recvonly, with no external ICE servers, and gathers host candidates for at most three seconds before POST.
- `POST /close` accepts `{request_id, session_id?}` and must idempotently cancel only that pending/active request. A pending request ID is essential: an aborted offer may otherwise create a worker whose response/session ID the browser never receives. When a session ID is present, the server must also verify it. Server-side pending/session deadlines are required because page-exit cleanup delivery is not guaranteed.

Failed/aborted attempts close the browser connection and await `/close` for at most five seconds before fallback. If that cleanup cannot be confirmed, the next attempt fails closed. The successful session transfers to the page after verifying the permitted actual codec and at least five decoded/presented frames. It continues to display video and statistics until Disconnect, transport failure, page exit, or its 120-second diagnostic deadline. The native receiver allows six seconds after the first frame for those five frames, accommodating the native idle refresh at1fps within the unchanged12second attempt bound. Pixel motion is reported separately as `pixelChangeObserved`: false is valid for a static console, and must not be called dynamic-content validation. The standalone canvas test still requires changing pixels. It retains the last actual received image after disconnection.

This local native receiver does not by itself provide Internet reachability, identity pairing, remote authorization, authenticated signaling through an untrusted relay, TURN, adaptive media or desktop input. Full SDP remains only in the local HTTP exchange, not the downloadable report. Browser loopback results remain in separate evidence and cannot substitute for a native sender run.


The native adapter has a stricter profile contract than the generic loopback policy. `nativeCodecPreferences` / `createNativeCodecPlan` accept H.264 Constrained Baseline level 3.1 or greater with packetization-mode 1, using the equivalent profile/constraint patterns in [RFC 6184 table 5](https://www.rfc-editor.org/rfc/rfc6184.html#section-8.1). An absent explicit H.264 profile, Main/High-only advertisement, mode 0 or a lower level fails closed as `unsupported_profile`. The native HEVC encoder requires Main profile in profile-space 0, tier 0, SRST, and level 4 or greater. Missing HEVC profile/tier/tx-mode use the defaults defined in [RFC 7798](https://www.rfc-editor.org/rfc/rfc7798.html#section-7.1); missing/low level does not meet this encoder's requirement. Main10-only capability is not substituted for Main1. Duplicate/malformed FMTP parameters and unknown level values are rejected. Browsers that advertise only a lower HEVC level fall back to compatible H.264 with `browser_h265_profile_unavailable`.

The browser filters original advertised capability objects before its offer and checks the actual inbound RTP codec profile continuously. The server must independently bind the RTP payload type to the negotiated compatible profile and validate actual encoder SPS/profile/level; a MIME match or successful browser decoding cannot substitute for those checks. An H.264-only diagnostic preference changes selection, not the native capability report. Generic loopback preferences remain unchanged and are not proof of compatibility with the native encoder's narrower formats.


Verified on 2026-10-08: actual native VM → WebRTC → Chromium154 H.265 and Constrained Baseline H.264, plus Safari26.4 Automatic correctly falling back to H.264 because its HEVC level93 is below this encoder's required120. These current runs also recovered after deliberately dropping the first complete keyframe, with actual PLI/FIR forwarded to the native encoder, and confirmed explicit closes. See [native evidence](../docs/evidence/native-webrtc.md). Earlier successful decodes with mismatched profiles are retained there as superseded evidence, not accepted conformance. This does not certify every browser/device, Internet security or target frame rate. The standalone canvas probe retains its separate [loopback evidence](../docs/evidence/browser-codec-policy.md).
