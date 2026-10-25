# 0003 — Typed media-session signatures and local Keychain identity

Date: 2026-10-08. Status: implemented security core with real cryptographic and local Keychain tests; live media integration belongs to the separate media service validation. Not an Internet or end-to-end-encryption release acceptance.

## Decision

Use P-256/SHA-256 identities compatible with browser WebCrypto and Apple's Security framework. Host private key remains in the login Keychain, marked non-extractable and checked after creation/open/signing. Public device identity is a validated uncompressed SEC1 point. Private browser storage and authenticated client-code distribution remain endpoint responsibilities.

Explicit local approval binds the full host/device public keys, fresh pairing nonce, exact VM and known scopes. Its independently compared digest is supplied as a local command-line argument. Grants are host-signed and versioned; revoked grant versions remain in the private registry. This records the administrator's decision, but does not prove a person actually compared the channels. No remote approval, silent public-key replacement, or signaling-service TOFU is introduced.

Use fixed typed canonical bytes instead of signing JSON serialization. A versioned domain prefix separates pairing, grant, offer, answer, confirmation and close. Full original SDP bytes are SHA-256 hashed, and a strict SHA-256 fingerprint binding is separately included. The initial profile accepts a single effective DTLS identity across all media sections and rejects mixed hash algorithms/conflicting fingerprints. It preserves CRLF/LF exactly for the digest. This is a deliberately constrained profile of [RFC 8122](https://www.rfc-editor.org/rfc/rfc8122.html), not a general SDP parser.

A fresh host challenge is signed by the paired device in its offer intent. Signature, current grant and exact offer are verified before atomic challenge consumption. The host signs an answer transcript that includes the complete offer intent, exact answer digest/fingerprint and session ID. The device confirms that transcript with another signature. The media service must verify the signed answer before giving those exact bytes to `setRemoteDescription`, and the host must wait for confirmation before encoding. DTLS fingerprints must be bound to the authenticated endpoint; simply sending them through signaling would permit substitution, as discussed in [RFC 8827](https://www.rfc-editor.org/rfc/rfc8827.html).

The library returns revoked pending/active IDs and exposes current-session authorization checks. Actual peer/encoder termination is the transport's responsibility. Input additionally needs the existing current VM control lease/epoch; an observe signature never grants input, files, or execution. Challenge lifetime is bounded to 60 seconds and nonce state to 256; fresh process instance randomness invalidates all old-process challenges. Active media lifetime is separately enforced by its transport.

## Keychain evidence and consequence

The first real test correctly failed: placing `kSecAttrIsExtractable=false` only in the private-key subdictionary did not produce a non-extractable key on this Mac. Its generated test key was deleted. Apple's [file-keychain implementation](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_keychain/lib/SecKey.cpp) reads this particular attribute from the top-level parameters in `GetKeyParameters`. Setting it at both levels produced a key whose actual attribute is false; P-256 signing and reopening the identity succeeded. No private material was exported or printed.

A subsequent actual test found that a different unsigned executable cannot sign using that identity: `CSSMERR_CSP_OPERATION_AUTH_DENIED` (-25293). We retained the failure and did not modify the ACL or allow prompts. The production-path choice is to expose local admin CLI dispatch as library code inside the **same frozen media executable** that subsequently signs sessions. Same-binary init/approve/revoke across separate invocations passed; the separate test executable's denied signing is retained as a negative assertion.

[Apple TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains) distinguishes file-based Keychain ACLs from data-protection Keychain access-group entitlements. The latter's packaged/provisioned entitlement requirements are not available in this unsigned CLI setup. This implementation uses the public SecItem/SecKey APIs with the login Keychain; it does not claim Secure Enclave storage, automatic unlock, signing-upgrade continuity, or cross-machine key recovery. Stable code signing and explicit local access recovery remain release gates.

The adapter explicitly disables Keychain UI for its calling process, fails if interaction would be needed, verifies [the extractability attribute](https://developer.apple.com/documentation/security/ksecattrisextractable), and signs through [SecKeyCreateSignature](https://developer.apple.com/documentation/security/seckeycreatesignature(_:_:_:_:)). Apple's DER signatures are converted using the P-256 crate into the 64-byte `r || s` representation specified by [WebCrypto ECDSA](https://www.w3.org/TR/webcrypto/#ecdsa-operations).

## Reproduction and boundaries

See [the crate wire and CLI contract](../../crates/session-auth/README.md), its checked-in public golden fixture, protocol tests and explicitly opted-in real Keychain integration test. Original Keychain failures are preserved in [the evidence](../../archive/docs-evidence/session-auth/keychain-initial-failures.json). State files are private, bounded, locked and atomically replaced; they contain public identities and grants, not private keys or SDP bodies.

These tests establish cryptographic binding and local access behavior. They do not test hostile signaling over the Internet, direct-versus-TURN negotiation, cloud confidentiality, browser code provenance, persistent revocation transport cleanup, or real desktop traffic after authenticated negotiation. Those require separate end-to-end tests through the actual media path.
