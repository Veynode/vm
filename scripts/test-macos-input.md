# Actual macOS input acceptance

This fixture does not authorize input, change TCC, start a VM, launch an app,
or send host UI events. The operator must first approve Guest Accessibility and
start the service with input enabled. Existing private-root file access is used
only to retrieve the oracle's own bounded event snapshot. No command scope is
needed. The oracle must remain focused with no concurrent human/other-agent input.

Launch the **fresh** staged oracle from Guest Terminal after that approval. Replace
the nonce with a new 32-character lowercase hex string for each run, and use new
filenames inside the already configured file root:

```sh
"$HOME/veynode/macos-input-oracle-stage3" \
  --run-id 0123456789abcdef0123456789abcdef \
  --log "$HOME/veynode-files-fixture-stage1/files/input-0123456789abcdef0123456789abcdef.jsonl" \
  --snapshot "$HOME/veynode-files-fixture-stage1/files/input-0123456789abcdef0123456789abcdef.json" \
  --seconds 120
```

Then on the host, using the chosen frozen controller and running daemon:

```sh
python3 scripts/test-macos-input.py \
  --binary /ABSOLUTE/FROZEN/vm-control --state-dir "$PWD/.local/control-macos" \
  --id macos-smoke --run-id 0123456789abcdef0123456789abcdef \
  --oracle-snapshot input-0123456789abcdef0123456789abcdef.json \
  --wait-ended 150 --output "$PWD/.local/evidence/macos-input-NEW.json"
```

The script refuses unavailable input before sending anything. It checks the nonce,
current key-window state, geometry, and actual JPEG header dimensions; obtains a
fresh screenshot for each click, key, modifier chord, drag and
scroll; then requires corresponding *new* AppKit receiver events. Physical-pixel
mouse positions must match pixel centres within one pixel. AppKit may coalesce
drag/scroll events. `CGEventPost`/controller dispatch ACK is explicitly not delivery.

The screenshot is never stored. Only physical keycodes/flags and the oracle's own
window events are logged (no character text). CGEvent source metadata is not
trusted or treated as authenticated origin. There is no claim of Unicode/layout,
AX tree, browser input, broad application compatibility or timing performance.

Each snapshot is written privately and atomically replaced, with a nonce and
increasing revision. It is <=256KiB, contains <=513 event records, and preserves
explicit dropped/write-failed/observation-gap state. A reader whose retained inode
is replaced can fail the production file worker's unchanged/nlink checks. Only
such `changed` read failures are retried, at most six attempts within three seconds;
partial transfers are cancelled. No file-worker security check is weakened.
Every successful download verifies <=32KiB chunks, sequential offsets, declared
size and whole-file SHA256. This is atomic visibility, not storage durability.

After the positive actions, the fixture waits5.3s for an anchor to expire and
requires refusal with no new oracle events. This validates stale-anchor refusal.
The oracle exits on its own bounded timer. Its real terminal summary must report
no held keys/buttons/modifiers, lost focus, overflow or dropped log records.
The scoped MCP child must exit0 after EOF before the report can pass. Timeout or
forced child reaping is cleanup failure, not success. No shutdown/force-stop,
permission changes or injected close shortcuts are performed.

## Offline checks and staging

```sh
native/guest-macos/build-input-oracle.sh --output "$PWD/.local/guest-share/macos-input-oracle-NEW"
.local/guest-share/macos-input-oracle-NEW --self-test
python3 scripts/test-macos-input.py --self-test
```

Stage3 SHA256: `9fb7c1977b3c04bd646b9df97afa269b30457be498aee6ee8e686103e7aef2a6`.
The Swift self-test verifies geometry including nonzero display origins, genuine
bounded private files, atomic snapshot replacement/retired inode, nonce metadata,
and tracked input model behavior without running AppKit or capturing input.
Python checks reject wrong nonce/revision/gaps, wrong geometry and dispatch-only
or wrong-coordinate/key evidence. These are fixture checks, not Guest acceptance.
Stage2 and running services are unchanged. Actual Guest acceptance remains pending.
