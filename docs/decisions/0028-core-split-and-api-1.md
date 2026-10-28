# 0028 — One core behind local API 1; the CLI, MCP, web UI and Veynode core are its clients

Date: 2026-10-28. Status: **accepted direction**, implemented in stages on `refactor/core-split`. Supersedes two points
of [0027](0027-veynode-core-mac-engine.md): VeynodeVM is also released as a product of its own, and Veynode core drives
it through the daemon's socket API rather than `veynodevm --json`.

## Context

The daemon (`vm-control`) knew only VMs. Containers, machines, checkpoints, images, macOS containers and the shared
engine lived in the `veynodevm` CLI, which wrote their records and disks itself. Every other client went through the
CLI: the MCP `container_*` and `image_*` tools ran `veynodevm --json`, the web UI ran it for every action and its
terminal and desktop streams, and the daemon ran `veynodevm __desktop`, `__driver` and `__proxy`. The CLI in turn
linked the whole `vm-control` crate for its internals.

Core's draft plugin (`packages/plugins/veynodevm` on core's `feat/veynodevm`) was written against a "local API 1" that
the daemon did not serve: a `hello` handshake, error codes, binary frames for streams and `machine.*` methods.

## Decision

1. **The daemon owns every piece of state.** Machine, container, checkpoint, image, macOS container and engine records,
   disks and keys are read and written only by `vm-control`. A client never opens the state directory beyond finding
   the socket.
2. **Local API 1 is the one interface,** specified in [docs/api.md](../api.md) and implemented by the
   `veynodevm-api` crate (`crates/api`): the wire envelope, error codes, frames, `hello` and a blocking client. Core's
   plugin, the CLI, the MCP server and the web UI are all clients of it. Methods without a namespace (`status`,
   `exec_start`, `screenshot`, …) are the VM layer and keep their names.
3. **Dependency rule.** No client depends on `vm-control`: clients (the CLI, the web UI, the core plugin) depend on `veynodevm-api` or speak API 1 themselves. Only the core's own media side (`vm-media-local`, the console broker) links `vm-control`. `scripts/check-changed.py` checks this on every manifest change.
4. **Two releases, one archive.** The same signed archive is installed as a product of its own (CLI, daemon, MCP, web
   UI, native console) and by core's installer (`install.kind: "unpack"`), which uses the daemon and the CLI. Both
   share one home and one daemon; `hello` carries the API and daemon versions, and the daemon migrates its state
   files on start. `scripts/package-release.py` assembles that directory (`bin/`, `libexec/`, `VeynodeVM.app`,
   `share/boot/`, `web/`, `install.py`, `MANIFEST.json`); it signs ad hoc unless given an identity and never
   notarizes or publishes. The QEMU/GPU runtime stays out until its redistribution is reviewed.
   `container.json` and `machine.json` carry `schema`
   ([`services/records.rs`](../../crates/control/src/services/records.rs)): an older record is upgraded when read,
   its original kept as `<file>.v<N>`, and a newer one is refused and left as it is.
5. **The web UI and the gateway are part of the standalone product**, reversing 0027's "not part of the 1.0 surface".
   Core still uses its own console, gateway and leases.

## Consequences

- API 1 is a compatibility promise to core. A change that breaks a request or result shape needs API 2 and a
  `hello` that refuses the old version with code `version`.
- The CLI's `--json` output is no longer a contract for programs; it stays for people and scripts.
- Machine names follow the daemon's `validate_id` rule, `[A-Za-z0-9_-]{1,64}`, which is the rule core's plugin already
  checks. Container names keep their own rule, which also allows `.`.

## For core's plugin

- It needs no change to the wire protocol it implements today. Its fake daemon in `tests/helpers/fake-daemon.ts` is the
  reference for the `machine.*` and `files.copy` shapes, and `crates/api/tests/vectors` holds the shared vectors both
  sides test against.
- `images.*` are served by the daemon as `image.*` (singular, like `machine.*`); the plugin keeps its own image records
  and may move to them later.
