# Original AU deadline ownership across media transport

Status: implemented and component-validated in `5c9b899`, independently audited
in `87c0541`, 2026-10-09. New-version real browser acceptance remains incomplete.
This decision does not change the original 250 ms budget or
turn a known expiry policy stop into successful continuous playback.

## Observed defect

The bounded NACK cache can retain a packet after the 128-entry AU lookup table
evicts its expired identity. A real localhost Peer regression reproduces this
with only one cached packet and 129 registrations: a delayed remote NACK triggers
`au_unknown`, not the required known-expiry outcome. The exact failing source,
command, exit 101 and limited transport evidence are preserved in
[the before archive](../evidence/media-au-provenance-before/README.md). A separate
[actual browser loss case](../evidence/media-nack-loss-live/v2-au-unknown/README.md)
also failed with unknown AU. It does not prove every detail of the synthetic
reproduction happened identically in that session.

## Chosen boundary

Obtain an opaque immutable proof of the original AU deadline before an original
packet enters the asynchronous driver queue. Bind it to the actual Peer, negotiated
primary/RTX mapping and timestamp. Retransmissions share that proof; they cannot
mint a new age budget. Each in-flight packet owns a separately bounded attempt.
Cache retention, pending attempts and AU lookup records have different lifetimes
and must expose separate bounded accounting.

Carry owned context through the RTC core, encryption, output queues and TURN
permission buffering to the final socket gate. Actual packet destruction releases
its context. Final admission still parses real wire data and checks the proof's
issuer and permitted identity; untrusted metadata alone cannot authorize a send.
GSO requires one proof per media segment. Pending polls and non-GSO retry must
reuse the original attempts, not consume a new permit or extend the deadline.

The implementation vendors the pinned RTC core to add owned-context APIs while
preserving its existing general-purpose interfaces. The product deadline path
must consistently use the owned APIs. A compatibility method that discards context
cannot silently become a fallback on this path.

TURN wrapping must keep actual buffer ownership. Where the upstream synchronous
TURN API produces fresh bytes, the bridge must bound output draining and match
the complete inner encrypted datagram to the original owned attempt. Missing,
duplicate or unexpected media output is an explicit failure, never a long-lived
best-effort header lookup. No encrypted payload is logged for this matching.

## Rejected shortcuts

- A larger AU table delays recurrence without matching packet lifetime.
- A cache-only deadline misses original or repair packets already in core/driver
  queues before final send starts.
- A final-socket-only side table cannot observe upstream drops: SRTP errors,
  missing ICE routes, and TURN permission failure/eviction can destroy packets.
  Clearing such a side table is not proof that packet-owned references retired.
- Unknown identity must not be allowed through, and retransmission must not
  refresh the original deadline.

## Required evidence

The [component record](../evidence/media-au-provenance-component-v1/README.md)
preserves the failing regression and the corrected original-deadline assertions.
Recorded checks passed: 104 wrapper tests, 42 core handler tests (a subset of the
upstream suite), five actual localhost Peer tests, and 157 main tests with eight
existing explicit ignores. These cover queued originals without NACK, remote
NACK after retirement, proof binding and capacity, drop paths, TURN encapsulation,
GSO, Pending/error/cancellation and close. The 129-registration Peer regression
now reports known expiry with zero unknown/rejected datagrams; close releases
129 proofs and three attempts, with all live ownership gauges zero. Its real
Peer transport is non-RTX and does not decode video; RTX is separately covered
by wrapper component paths.

The [independent audit](../evidence/media-au-provenance-audit-v1/README.md)
verified frozen artifact/source/log hashes and reconstructed the exact vendored
RTC tree from the published archive plus patch. It did not rerun the tests or
establish a reproducible binary build. Logical ownership bounds do not measure
all RTC allocations or RSS.

Still required: actual hardware-encoded Guest/browser direct and local TURN checks for
H264 and supported HEVC. Existing normal tests generated no repairs and cannot
substitute for a loss case. A correctly classified expiry can establish safe
termination but does not pass finite-loss continuity/recovery acceptance. Any
later policy to suppress already expired repairs needs its own explicit decision,
counters and tests; it is not part of this provenance repair.
