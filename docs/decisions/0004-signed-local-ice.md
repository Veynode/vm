# Signed ICE bootstrap for bounded local transport validation

Implemented 2026-10-09. This is an opt-in local transport slice, not an Internet endpoint or Cloudflare deployment.

The existing paired host/device identity and per-VM grant authorize a separate, one-use ICE bootstrap before `createOffer`. `POST /auth/ice-config` accepts a device signature over `IceRequest { challenge, request_nonce }`. The challenge binds host/process instance, device, VM, sorted scopes, grant version, nonce and a60second window. Current grant checks and signature verification precede atomic challenge consumption; successful retrieval cannot reuse its challenge for an offer. A subsequent offer needs a new challenge. Admission is limited to8 requests per device per minute with256 retained device entries; the existing challenge store is independently bounded.

The response `{request, config, signature}` is host-signed over both canonical request and full configuration digests. The browser checks the pinned host, exact request, current grant context, time, signature and strict configuration schema before using it. This does not grant any additional VM scope. Revocation blocks later bootstrap and existing session authorization still terminates media/control. It cannot revoke an already issued TURN credential by itself; issuer revocation/expiry is a separate integration.

`--ice-config PRIVATE_JSON` requires authenticated mode. The file must be a single-link regular file owned by the service user, with no group/world access, no final symlink, and at most16KiB. It is reopened for each bootstrap and peer admission, permitting trusted atomic credential refresh. It contains only pre-minted short-lived credentials, never an issuer/mint secret. TTL metadata is a trusted local declaration: an arbitrary password does not prove issuer-enforced expiry. A real coturn expired-HMAC test covers this distinction.

The current URL policy deliberately permits only explicit IPv4 loopback ports: `stun:127.0.0.1:PORT` or `turn:127.0.0.1:PORT?transport=udp`. There are at most4 entries and8 URLs total; duplicate URLs, mixed schemes, STUN credentials, missing TURN credentials, and relay without TURN are rejected. External hosts, IPv6, TCP/TLS TURN and DNS are unsupported in this slice. The default remains no ICE servers. `all` permits direct candidates; `relay` requires relay candidates. No public listener is introduced.

Configuration version1 contains `transport_policy`, `issued_at`, `expires_at`, and `ice_servers`. Its structural TTL is1..600seconds; peer/browser admission requires at least210seconds remaining to cover the current bounded diagnostic lifetime. This is not a production session-duration promise. The browser keeps credentials out of diagnostics; `/capabilities` only advertises `authentication.ice_config_enabled`. The credential response is `Cache-Control: no-store`. Local HTTP signatures provide integrity/authenticity, not signaling or credential confidentiality. A future Internet endpoint requires authenticated TLS/client-code provenance and independently validated deployment/abuse policy.

Canonical encodings are fixed and domain-separated, using the existing big-endian safe integer/text rules:

- `Veynode/ice-config-request/v1\0`: complete Challenge fields, then request nonce.
- `Veynode/ice-config/v1\0`: version, policy, issued/expiry, server count; each server has URL count/texts, username text and credential text. Omitted STUN strings encode as empty strings; JSON null is invalid.
- `Veynode/ice-config-response/v1\0`: lowercase SHA256 of request canonical bytes, then SHA256 of configuration canonical bytes.

Signatures are P-256 P1363 lowercase hex. The public synthetic fixture is `crates/session-auth/tests/ice-golden.json`. Complete offer/answer SDP and DTLS fingerprint binding is unchanged; signed configuration does not replace authenticated media negotiation.

Sources: [WebRTC ICE transport policy](https://www.w3.org/TR/webrtc/#dom-rtcicetransportpolicy), [TURN URI syntax](https://www.rfc-editor.org/rfc/rfc7065.html), [coturn sample configuration/REST credentials](https://github.com/coturn/coturn/blob/master/examples/etc/turnserver.conf), [Cloudflare credential generation and TTL guidance](https://developers.cloudflare.com/realtime/turn/generate-credentials/). Cloudflare has not been provisioned or tested.
