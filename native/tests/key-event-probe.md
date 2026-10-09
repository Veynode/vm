# Own-window modifier-event probe

This diagnostic distinguishes what reaches an AppKit application from what its
focused view receives. It does not change VZ routing, generate input, install a
global/local event monitor or event tap, request TCC, or change system shortcuts.
Only a foreground key window belonging to this app is observed. It records no
`characters` or `charactersIgnoringModifiers`. Only C, N, Return, Escape and the
modifier keycodes have values in the log. Other keycodes are omitted entirely:
`flagsChanged` still records modifier state with `test_key=redacted_modifier_event`
and no `key_code`; other event kinds only increment redacted handler counts. The UI has no text/password entry field.

The current VZ worker directly embeds `VZVirtualMachineView` with
`VZMacKeyboardConfiguration` and `capturesSystemKeys=true`. It does not override
`keyDown` or `flagsChanged`. The public VZ SDK describes `capturesSystemKeys` as
redirecting certain system shortcuts; it does not promise reconstruction of a
modifier press when an upstream synthetic event sequence omits it. The separate
focus fix4d1296c ensures key window/first responder but has only Linux window
lifecycle acceptance, not actual macOS modifier delivery. A plain character in
Terminal is therefore insufficient evidence for which layer lost its modifier.

Build without launching either desktop:

```sh
python3 native/tests/build-key-event-probe.py \
  --output-app "$PWD/.local/KeyEventProbe-NEW.app" \
  --guest-output "$PWD/.local/guest-share/key-event-probe-NEW"
```

Frozen stage6 app: `.local/KeyEventProbe-stage6.app`; matching Guest binary:
`.local/guest-share/key-event-probe-stage6`. Both executable copies have SHA256
`fab53eac02b9bc177de76749390fb86a817910de7996fa609cc4986609b44a3b`.
The app has a verified ad-hoc signature and a dedicated bundle identifier.

Root/operator can launch in the Guest, using a fresh private log filename within
the existing files root for later bounded MCP download:

```sh
"$HOME/veynode/key-event-probe-stage6" \
  --log "$HOME/veynode-files-fixture-stage1/files/guest-key-probe-01.jsonl" \
  --seconds 120
```

If the share was unmounted after local service deployment, remount only for this
diagnostic or copy this new binary through an explicitly chosen deployment step.
The local production service bundle does not depend on the probe/share.

For a direct host AppKit comparison, the operator must explicitly select the
host-only diagnostic mode and operate this particular app via CUA:

```sh
"$PWD/.local/KeyEventProbe-stage6.app/Contents/MacOS/key-event-probe" \
  --host-diagnostic --log "$PWD/.local/evidence/host-key-probe-01.jsonl" \
  --seconds 120
```

Bring the exact probe window forward and click its content to focus the view.
Use the same CUA key spelling/sequence for plain C, Ctrl+C, plain N, Command+N,
and Return on both surfaces. Record the exact operations separately from the
event log. Each run exits automatically after1–120seconds, or on window close,
and writes a terminal summary with flush. Wait for process exit before retrieving
the0600 log. Logs have a128KiB/256 ordinary-record limit with reserved final space.

`stage=application` records own-key-window `NSApplication.sendEvent` input;
`stage=view` records `keyDown`, `keyUp`, `flagsChanged` and key-equivalent handling.
The latter returns false rather than overriding shortcut behavior. A single key
can generate multiple records; counts are not a number of unique OS events.
Focus changes include app-active, key-window and exact-view-first-responder flags.
Device-independent flag bits are reported separately for Control/Command/Shift/
Option/CapsLock/Function. No event source metadata is treated as trusted provenance.

Interpretation:

- No Control flag or modifier transition even in direct host application input
  narrows the failure to the supplied CUA event sequence/host handling before VZ.
- Correct host probe flags, but missing Guest application flags, narrows it to
  the VZ-targeted host window/event forwarding/virtual keyboard path. Different
  target apps remain a limitation; this alone does not prove a VZ framework bug.
- Flags at the Guest application entry but not the Guest view point to local
  responder/key-equivalent routing; check recorded focus before concluding.
- Correct flags in both Guest stages but Terminal not reacting requires a
  separate Terminal mode/application investigation, not blind VZ rewrites.

Host `--self-test` passed whitelist/flag serialization, redaction, bounded real
JSONL/private-mode/final-record and existing/symlink refusal. Default physical-host
launch was rejected before window creation, with no log. Evidence is in
`.local/evidence/key-event-probe-stage4/`. No actual CUA/Guest modifier acceptance
has been performed by this implementation task.

The earlier stage2 self-test **failed**: Foundation's
`resolvingSymlinksInPath` retained the `/var` alias for `NSTemporaryDirectory`,
and the real log open correctly rejected that component with `O_NOFOLLOW_ANY`
(`ELOOP`,62). An unchecked compound shell initially masked that exit status;
the original invalid report and corrected failure are retained under
`.local/evidence/key-event-probe-stage2/`. Stage3 exposed the exact error.
Stage4 resolves only the self-test temporary parent with POSIX `realpath`;
production log path checks remain unchanged. Its independently checked
subprocess test exited0. The build script now runs the same self-test with
`check=True` before publishing a fresh app, so a failed test cannot publish.
Existing frozen stage1–3 artifacts are not overwritten. No Guest input outcome
is inferred from these host-only tests.

Stage4's subsequent **actual host UI launch failed** with SIGTRAP: directly
constructing `KeyApplication()` did not establish AppKit's singleton, so framework
startup attempted a second NSApplication. The crash remains in
`.local/evidence/key-event-probe-live-v1/host-crash-summary.json`; the successful
metadata/file self-test never exercised AppKit startup. Stage5 calls
`KeyApplication.shared` early, checks its actual subclass and declares the same
Objective-C principal class in the app plist. Both the bundled and standalone
stage5 executable completed separately authorized real host `--seconds 1`
window lifecycles, each exit0 with started/deadline-ended records and no keyboard
events. Results are in `.local/evidence/key-event-probe-stage5/`. This is lifecycle
validation only; actual CUA/Guest modifier delivery remains for the operator's
paired test. Stage4 artifacts and crash are preserved.

Root's subsequent actual paired stage5 CUA test used `c`, `ctrl+c`, `n`,
`super+n`, `Return`. In `key-event-probe-live-v1/host-v2.jsonl`, host application
and view saw Control262144 for C and Command1048576 for N; the final summary had
8 redacted handler occurrences and no whitelisted flagsChanged records. In
`guest-01.jsonl`, both corresponding C/N events had flags0 at application/view
with focus true; its redacted count was0. Both logs ended by window close.
Host exit0 was observed; Guest shell exit was not reliably retrieved. This
narrows the difference but does not locate where the modifiers were lost.

Stage6 therefore preserves all own-window flagsChanged modifier states, even
when the keycode is not whitelisted, while omitting that keycode completely.
Pure tests exhaust all65536 keycodes, retaining only the14 approved values and
checking unknown codes cannot leak through keyDown/keyUp/keyEquivalent or the
flagsChanged schema. Real bounded log/permission tests and physical-host default
refusal passed (`.local/evidence/key-event-probe-stage6/`). Stage6 has not itself
been exercised with actual CUA events by this implementation task.

A possible next VZ boundary diagnostic is an explicitly enabled subclass of
`VZVirtualMachineView` that logs these same bounded own-window fields at
keyDown/keyUp/flagsChanged/key-equivalent entry and then calls the original
super implementation unchanged. It must not invent modifier transitions or
rewrite events. That needs a newly launched worker; it cannot be safely injected
into the current frozen worker. A RAM Linux VM can check that host events reach
the VZ view, but its VZUSBKeyboard differs from macOS VZMacKeyboard and its shell
is not the Guest AppKit probe, so it cannot establish macOS modifier forwarding.
No VZ production routing code has been changed for this diagnostic.
