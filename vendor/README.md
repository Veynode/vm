# Vendored RTC dependencies

## webrtc 0.21.0

Source: crates.io `webrtc`0.21.0, repository <https://github.com/webrtc-rs/webrtc>, original VCS commit `4be2e3d6f99787ac438d28f9f704d343b1aa1498` from `.cargo_vcs_info.json`.

The published `.crate` SHA256 is `b3cc7db94f14a20c9401202f7300f38071dcac4446177b40e12dc2df34b2256b`, matching the former registry entry in the workspace lockfile. The source archive is available at <https://static.crates.io/crates/webrtc/webrtc-0.21.0.crate>. Original MIT and Apache-2.0 license files and notices are retained; this does not choose a license for Veynode's product code. `.cargo-ok` (local Cargo cache marker) is omitted.

The original TURN teardown change modified three upstream source files:
`peer_connection/mod.rs`, `peer_connection/driver.rs`, and
`peer_connection/transport/turn_relayer.rs`. That historical change is preserved
in `patches/webrtc-0.21.0-turn-cleanup.patch`; it is not the complete current
vendor diff. Subsequent changes include the media stop fence, AU deadlines,
bounded NACK responder and transport tests. Current tracked source and commit
history are authoritative for their scope. The root `[patch.crates-io]` selects
this copy. The vendored crate is excluded from workspace membership.

### Original TURN teardown patch

Upstream's normal close clears TURN transactions and immediately aborts the driver, leaving allocations alive. The patch requests authenticated Refresh with LIFETIME=0 using the existing relay implementation, retains the same UDP socket, and checks exact source tuple, transaction, method, zero lifetime inside the HMAC span, and MESSAGE-INTEGRITY before reporting success. See [RFC8656 section8](https://www.rfc-editor.org/rfc/rfc8656.html#section-8) and [RFC8489 section14.5](https://www.rfc-editor.org/rfc/rfc8489.html#section-14.5).

The cleanup driver is bounded to750ms; `PeerConnection::close` allows1250ms (1500ms for the existing dedicated-reactor option) to finish and returns `TURN allocation cleanup unconfirmed` on failure. Media's existing close acknowledgement already includes this result. In-flight Allocate replies are drained during that bound, and discarded allocations/Allocate timeout uncertainty remain sticky failures. No application dispatch resumes during teardown.

This is a narrow normal-close patch, not an audit of every TURN operation. Abrupt process death/Drop cannot attest cleanup. A lost response, stale nonce, unavailable server, or allocation that finishes after the deadline can require server expiry; there is no false success. General server replacement/ICE restart cleanup is not repaired; it is marked unconfirmed where the relayer discards an allocation. Products currently keep configuration fixed within a peer. A success authenticates the server's zero-lifetime acknowledgement; actual coturn quota deletion may occur on its later timer. The real low-quota test separately observes server deletion before creating the next peer.

Validation and reproduction: `docs/evidence/turn-allocation-cleanup.md`. Future upstream upgrades must recheck this patch and rerun real low-quota and forged-ACK tests. No upstream issue/PR has been sent.

## rtc 0.21.0

The owned AU provenance repair also vendors the RTC core. Its published archive
is <https://static.crates.io/crates/rtc/rtc-0.21.0.crate>, 7,674,756 bytes,
SHA256 `a0c5e4a8791908797789d63efd013d6987b1f87cce4255b94343d1fa9c69a81f`.
An independent local audit matched that digest to the cached crates.io index and
the registry entry in source commit `e65dfc9`'s workspace lockfile. All 364 package
files matched the local registry extraction before implementation differences.
The original MIT/Apache-2.0 license files and notices are retained.

Package metadata names <https://github.com/webrtc-rs/rtc> and VCS commit
`c8321cab79ae745aad973d62034ec84d301e893c`; this identifies the package's declared
source, not a separate GitHub verification. The local `.cargo-ok` cache marker
is not part of the published source archive.

The new core path carries opaque locally owned context from RTP submission,
through interceptors and encryption, into an owned output datagram. This allows
actual packet drops to release deadline provenance instead of leaving an
unobserved lookup-table entry. General-purpose unannotated APIs remain compatible;
the product deadline path must use the owned APIs. See
[decision 0016](../docs/decisions/0016-media-au-provenance.md) for the defect,
scope and required validation. Implementation and component validation completed
in `5c9b899`: 104 wrapper, 42 core handler subset, five actual localhost Peer and
157 main tests passed; eight existing main tests remain explicitly ignored.
The [component record](../docs/evidence/media-au-provenance-component-v1/README.md)
distinguishes actual non-RTX Peer checks from RTX component tests and preserves
the original failure. Known expiry remains a policy stop, not continuous playback.

The complete RTC change is preserved in
[the owned-context patch](patches/rtc-0.21.0-owned-packet-context.patch).
[Independent audit `87c0541`](../docs/evidence/media-au-provenance-audit-v1/README.md)
verified the frozen hashes and replayed this patch against the original archive:
all 365 resulting files match the committed vendor tree, including unchanged
licenses. This was an offline source reconstruction, not a reproducible binary
build. New-version real Guest/browser acceptance remains incomplete; these checks
do not resolve the separate QEMU/UTM/commercial-component distribution gates.
