# Local macOS Guest service bundle

This is a reversible foreground launcher and local installer. It does not install
launchd, change TCC or system settings, run a shell command supplied over the
network, stop existing services, or claim unattended boot recovery. Install once
from the read-only share; the installed bundle then has no dependency on that
share remaining mounted. The Guest needs no compiler, Python or other developer
tools. The two actual services remain the existing integration and capture code.

Build a new bundle on the host, preserving frozen source binaries:

```sh
python3 native/guest-launcher/build.py \
  --integration "$PWD/.local/guest-share/macos-guest-agent-windows-stage2" \
  --capture "$PWD/.local/guest-share/macos-capture-stream-console-stage4r1" \
  --output "$PWD/.local/guest-share/macos-services-NEW"
```

The output contains only `launcher`, `integration`, `capture`, `manifest.json`.
The manifest pins byte counts and SHA256 of all three executables. The complete
verified output directory is published using Darwin `renamex_np(RENAME_EXCL)`;
an existing output, including a concurrent publisher, is never replaced. Build
inputs must be regular, single-link files with no symlink path components.

The tested frozen bundle is `.local/guest-share/macos-services-stage3`:

| File | SHA256 |
| --- | --- |
| launcher | `4e53f10eca10878d9db3292814d1cfc77f135b76e138341d6bc40b24ee52e46c` |
| integration | `8328097f75a8f89f4668625780e189df89dec171f647623d8d7e3b902c664b96` |
| capture | `d16479bf819a519603bbd574e7d77bb2a4f4d3e5f6a76a408b979cce5b667562` |

## Guest installation and preflight

Use the already authorized Guest Terminal, as the ordinary console user. The
initial share mount is still required to obtain this bundle. Once mounted:

```sh
mkdir -m 700 "$HOME/veynode-local-services-v1"
"$HOME/veynode/macos-services-stage3/launcher" install \
  --destination "$HOME/veynode-local-services-v1/bundle1"
"$HOME/veynode-local-services-v1/bundle1/launcher" verify
"$HOME/veynode-local-services-v1/bundle1/launcher" preflight integration \
  --files-root "$HOME/veynode-files-fixture-stage1/files"
"$HOME/veynode-local-services-v1/bundle1/launcher" preflight capture
```

Do not repeat `mkdir` or install over an existing destination; reuse a verified
private parent and choose a fresh child name. The destination parent must already
be owned by this UID with mode0700 and no extended ACL. Installation rejects
literal `.`/`..`, repeated separators, symlink components/destinations, unexpected
file types, changed hashes, extended ACLs and non-private ownership/modes. It
writes private exclusive files, verifies the copy, syncs it, then atomically
publishes the directory. An error after successful publication is explicitly
`installation_published_durability_unconfirmed`; it does not delete the published
bundle or pretend to roll it back.

`verify` and installation do not query TCC or start either service. Preflight
first rejects root or a physical Mac, then calls only existing-permission checks.
Screen Recording permission is required. `--input` is a separate opt-in and also
requires already granted Guest Accessibility; omit it while permission is pending.
There is no execution option. Moving or renaming the service executable can alter
TCC attribution: a successful launcher preflight is not proof that the subsequently
exec'ed service inherits permission. Actual Guest `info`/capture acceptance remains
necessary. No quarantine attribute is removed; quarantined bundle files are refused.

After separately retiring the old same-port service through its ordinary lifecycle,
run in one Guest Terminal:

```sh
"$HOME/veynode-local-services-v1/bundle1/launcher" start integration \
  --files-root "$HOME/veynode-files-fixture-stage1/files"
```

In another Guest Terminal:

```sh
"$HOME/veynode-local-services-v1/bundle1/launcher" start capture
```

The launcher verifies the entire bundle, checks permission, acquires a per-UID,
per-role exclusive lock under `~/.veynode-services/`, and directly execs the fixed
binary/arguments. This replaces the launcher process. Its lock FD deliberately
survives exec and belongs to the actual service until exit; stale lock files need
no deletion. The integration service's existing private file worker uses
`POSIX_SPAWN_CLOEXEC_DEFAULT`, so it does not retain this inherited launch lock.
An old manually launched service does not hold this new lock; its existing vsock
listener still makes a competing service's bind fail. The launcher does not kill
it or probe by opening a potentially active stream.

`exec_attempt` always has `ready:false`. It attests an attempted verified exec,
not listener readiness, capture permission or health. Confirm readiness through
real MCP `vm_guest_info`/`vm_windows` and an authorized actual capture session.
The service's own exit status reaches the Terminal directly. Closing a Terminal,
logout or Guest reboot is not automatic service restoration; rerun the local
commands after login. The independent local directory means remounting the share
is unnecessary for this restart.

The private directories and manifest provide integrity checks within the trusted
same-UID deployment model. A user/process with that UID can replace the manifest
and binaries or race pathname exec; this is not protection against a hostile
same-UID account, a signed installer, sandbox, or Gatekeeper bypass.

## Host validation

```sh
python3 native/guest-launcher/test.py \
  --bundle "$PWD/.local/guest-share/macos-services-stage3" \
  --directory "$PWD/.local"
```

All14 tests passed in4.138s. They use disposable private host directories and an
exact owned `/bin/sleep` child only. Actual tests cover hash-preserving local
installation followed by removal of the source bundle, no-replace publication,
symlink/dotdot/repeated-separator/hardlink/FIFO/ACL/quarantine failures, strict
manifest types, physical-host rejection before TCC, duplicate flock ownership,
exec-child lock retention after the parent closes its FD, and availability only
after that child is reaped. Special lock files are rejected without blocking.
They do not start a Guest service, capture the host, change TCC, or prove actual
Guest local-bundle startup. Guest acceptance is separate.
