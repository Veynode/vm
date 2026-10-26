# Local pairing and session signatures

This crate authenticates typed pairing/session records. It does not provide a network endpoint, encrypt application bodies, terminate media on its own, or constitute an Internet/E2EE acceptance result. The transport must connect the checks to the **exact** SDP passed to WebRTC, gate encoder start on confirmation, apply current registry updates, close revoked transports, and enforce connection lifetime. Browser code distribution remains a trusted endpoint concern.

## Wire format version 1

Types are in `src/lib.rs`; unknown JSON fields are rejected. All binary values in JSON use exact-length lowercase hex without prefixes: public keys are uncompressed SEC1 P-256 (65 bytes), signatures are ECDSA P-256/SHA-256 `r || s` (64 bytes, WebCrypto compatible), hashes/nonces are 32 bytes. Compressed/invalid points, uppercase hex, duplicate/unknown/unsorted scopes, and integers above JavaScript's safe integer limit are rejected. Identifiers are 1–128 ASCII alphanumeric or `-_.`. Known scope order is `execute, files, input, lifecycle, observe`; requested scopes are a nonempty subset.

Canonical bytes start with ASCII `Veynode/DOMAIN/v1` and one NUL. `text(s)` is u32 big-endian UTF-8 byte length followed by UTF-8 bytes. Hex strings remain text, **not decoded bytes**. `integer` is u64 big-endian. A string array is u32 count followed by each `text`. There are no JSON hashes, floats, locale-dependent sorting, or Unicode normalization.

| Domain | Ordered fields |
|---|---|
| pairing | host_public_key, device_public_key, nonce, vm_id, scopes array |
| grant | host_id, device_id, device_public_key, vm_id, scopes array, version integer |
| offer | challenge fields (below), request_id, codec, offer.sha256, offer.fingerprint_sha256 |
| answer | offer fields **without another domain prefix**, session_id, answer.sha256, answer.fingerprint_sha256 |
| confirm | transcript_sha256 |
| close | session_id, transcript_sha256 |

Challenge field order: `host_id, device_id, vm_id, scopes array, grant_version integer, instance_nonce, nonce, issued_at integer, expires_at integer`. Times are Unix seconds; a challenge is valid for at most 60 seconds and belongs to one process instance. Codec is `h264` or `h265`.

The one encoding exception is identity derivation: SHA-256 of ASCII `Veynode/identity/v1`, NUL, then **raw** SEC1 public key bytes. Pairing digest is SHA-256 of canonical pairing bytes; transcript digest is SHA-256 of canonical answer bytes. The golden fixture contains public deterministic test identities, all signed records, and canonical hex. Test scalars in its generator are deliberately public; they must never be used as product identities.

`SdpBinding::from_sdp` hashes the complete original UTF-8 SDP (≤128 KiB); CRLF/LF are not normalized before hashing. Fingerprints must be SHA-256, exactly 32 colon-delimited hex octets, with the same effective fingerprint for every media section. Session inheritance is accepted, or every media section must carry a fingerprint. Mixed algorithms, conflicting fingerprints, missing bindings and malformed line endings fail closed. This deliberately supports one DTLS identity, not arbitrary multi-certificate SDP.

## Session ownership and revocation

`SessionGate` stores a random process nonce and bounded challenge/pending/active state. `accept_offer` checks exact SDP, current grant and real device signature, then consumes the challenge under one mutex. Invalid signatures do not consume valid challenges. Concurrent replay permits only one winner. `answer` consumes an opaque accepted offer and signs the full transcript. `confirm` verifies the device's signature over that transcript digest before making the session active. Confirmation is one-use. Signed close binds the session and transcript; unsigned closes must not reach authenticated sessions.

`replace_grants` checks host signatures and version monotonicity and returns affected pending/active session IDs. **The embedding service must stop those transports.** In-memory highest-seen versions prevent reinstalling revoked grants; durable registry tombstones preserve version progression across CLI operations. Same-account modification or rollback of the entire host state is inside the trusted-host boundary, not protection offered by these signatures. No automatic key replacement or identity recovery occurs.

The library does not turn an observe grant into input/execute/files authority. Transport-level media authorization requires observe; input additionally requires the input scope and a fresh screenshot anchor. Session expiration after confirmation is the embedding transport's responsibility; the 60-second challenge expiry is not a promise to terminate active video after 60 seconds.

## Local administration

`vm-pair init-host PRIVATE_NEW_DIRECTORY NAMESPACE` creates a Keychain identity and private registry. `public` and `list` use the same directory/namespace arguments. `pairing-digest` reads a Pairing JSON from stdin. To approve, locally compare the full pairing digest over an independent trusted channel, then run:

```
vm-pair approve PRIVATE_DIRECTORY NAMESPACE VERIFIED_PAIRING_DIGEST < pairing.json
vm-pair revoke PRIVATE_DIRECTORY NAMESPACE DEVICE_ID VM_ID
```

The command-line digest argument records an explicit administrator choice; it cannot prove that a human actually compared the channels. The browser must pin the approved host public key independently. No remote approve route is provided. Changing scopes requires revoke and a new explicit approval/version. Registry files are owner-only, bounded, flock serialized, and atomically published with fsync. No device private keys or SDP bodies are stored there.

`vm-pair verify` is a bounded crypto interoperability fixture, not an authorization endpoint. It reads tagged `grant`, `offer`, `answer`, `confirm`, or `close` JSON from stdin and reports `authorization_checked:false`. Its success does not check a live grant registry, nonce consumption, or transport lifetime.

## Keychain behavior

The adapter uses the public Security framework through `security-framework` with a few missing public constant bindings. It creates a permanent **file-based login Keychain** P-256 key, with extractability=false in both top-level and private-key dictionaries, and verifies the actual returned property. Apple’s CDSA file-keychain path reads this setting at the top level. Ambiguous duplicate tags fail closed. Public key export is permitted; private material is never exported or printed. ECDSA X9.62 DER output is converted to WebCrypto's fixed 64-byte format using the P-256 library.

The process owner must explicitly call `disable_interaction_for_process`; this disables Keychain prompts in that process only. Locked/unavailable/access-denied keys are errors. The code does not unlock, alter ACLs, enable iCloud synchronization, or claim Secure Enclave protection. A different executable or rebuilt ad-hoc identity may require explicit local authorization; see the retained cross-executable failure evidence. The actual different-executable test failed with `CSSMERR_CSP_OPERATION_AUTH_DENIED`; do not work around it by broadening the ACL. `vm_session_auth::cli::run_cli` allows a media binary to expose the exact same admin commands (for example `vm-media-local --pair init-host ...`) and then serve/sign with that **same frozen executable**. Same-binary cross-invocation init/approve/revoke passed, while a different test executable remained denied. Stable signing/packaging and key-access recovery remain release work.

## Reproduce

```
cargo test -p vm-session-auth --target-dir .local/stage-target
cargo clippy -p vm-session-auth --all-targets --target-dir .local/stage-target -- -D warnings
cargo run -p vm-session-auth --example generate-golden --target-dir .local/stage-target
.local/stage-target/debug/vm-pair keychain-self-test test-UNIQUE_RANDOM_NAME
```

The self-test creates and removes only its newly generated `test-` key, verifies the actual extractability property, signs/verifies, and reopens the same identity. No existing host key is deleted.

Real Keychain positive and negative acceptance (creates/removes one fresh test key):

```
cargo test -p vm-session-auth --test keychain --target-dir .local/stage-target -- --ignored --nocapture
```

## Signed physical console control

`control.rs` defines a separate `Veynode/control/v1\0` transcript and
`tests/control-golden.json` provides five cross-language fixtures. A signed request
contains the verified media session ID and answer-transcript SHA-256, a strictly
increasing sequence, an expiry at most five seconds ahead, and one typed action:
`file`, `observe_frame`, `click`, `drag`, `scroll` or `key`.
The canonical format uses UTF-8 length-prefixed strings and big-endian integers;
physical actions additionally bind the screenshot UUID,
source generation and Guest pixel dimensions. No command strings, file paths,
remote-selected VM IDs, Unicode injection or arbitrary QMP payloads are accepted.
`vm-pair verify` accepts `kind:control` for cryptographic interoperability only.

The host adapter (`crates/media-local/src/browser_input.rs`) requires a confirmed,
currently granted session, a private owner-held controller socket, a matching
QEMU console socket and worker incarnation. Observe and input remain separate
scopes; input needs the input scope plus a fresh screenshot. It allows one
request in flight and no queue. An explicit
PNG screenshot is the input anchor, retained at most five seconds after receipt;
RTP decoder stats and resized video pixels are not presented as that binding.
The daemon rechecks its own screenshot age, geometry and source generation
before every physical step. Capture validation and QMP dispatch occur in different
processes, so a small generation-change race remains; QMP acknowledgement does not
prove Guest application acceptance.

Session end, revocation or disconnect cancels the pending Unix RPC; the daemon
releases held keys and buttons at the end of every input operation. Cleanup is
not an application-level rollback or confirmation that every Guest has processed
key/button release.

This integration is confined to the existing loopback diagnostic server. Signed
HTTP input is authenticated but **not encrypted**. Internet deployment, authenticated
TLS/client delivery and end-to-end sensitive control transport remain separate
gates. macOS browser input is explicitly unsupported in this slice.
