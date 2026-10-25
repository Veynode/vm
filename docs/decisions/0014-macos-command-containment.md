# macOS command descendant containment: investigation and next gate

Status: investigation completed 2026-10-09; proposed experiment, **not implemented or accepted containment**. macOS async command execution remains unsupported. No Guest privileges, accounts, launchd jobs, entitlements, security settings or running services were changed during this investigation.

## Decision

Do not expose the existing macOS process-group runner as production cancellable MCP execution. A per-command launchd job does not supply the missing descendant boundary. The smallest useful next experiment is a **disposable macOS Guest**, with an explicitly installed privileged supervisor and one dedicated, otherwise unused non-root UID for each simultaneous command. Test cancellation independently of PGID and ancestry, including `setsid` and double-fork. This is a feasibility gate, not a decision to ship a privileged helper or a claim of cgroup-equivalent containment.

No supported rootless API was identified that both admits arbitrary shell/compiler subprocesses and guarantees termination of their escaped descendants. This is a scoped research result, not proof that no such mechanism exists. Retain the existing explicit unsupported capability until the required lifecycle semantics have an actual implementation and evidence. Do not silently substitute whole-VM termination: it would destroy unrelated same-console work.

## Current implementation and observed boundary

`native/guest-macos/main.swift` creates a private PGID with `posix_spawn`, retains the leader using `waitid(...WNOWAIT)`, kills the PGID before reaping, and bounds output and normal execution time. Its Darwin zombie-only `EPERM` handling avoids misreporting a dead leader as a live cleanup failure. These are useful identity/reaping protections; they do not prevent a descendant from moving to another process group/session. Existing [pinned cleanup evidence](../../archive/docs-evidence/apple-macos-guest-pinned-cleanup.json) expressly excludes deliberate process-group escape. Current info reports `async_exec:false`; the controller requires Linux async jobs rather than falling back to this legacy macOS runner.

Apple documents that `setsid()` creates a new session and process group. A child that is not already a group leader can therefore leave the original PGID; a subsequent fork can also remove the useful parent relationship. Waiting for the original leader or observing EOF on its output pipes is insufficient evidence that escaped work has stopped. [Apple setsid manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/setsid.2.html).

## Candidate mechanisms

| Mechanism | What it provides | Why it is insufficient by itself |
|---|---|---|
| Current PGID + pinned leader | Safe signaling of the owned group and leader reaping | Deliberate session/group escape remains possible. |
| Per-command launchd job | Supervisor/restart policy, startup credentials, normal termination handling | Documented descendant cleanup covers the same PGID, not all descendants after escape. |
| Dedicated per-command UID | Credential membership survives ordinary `setsid`, reparenting and double-fork | Requires privilege to establish; signaling success is not universal termination proof; credential changes, external services and concurrent creation require separate treatment. |
| Process enumeration / audit-token signaling | Inspection and potentially safer targeting of a known process incarnation | Enumeration is not an atomic admission/termination boundary and does not capture work delegated to an unrelated service. |
| App Sandbox | Supported capability restrictions for an app and its embedded helpers | No documented job-wide descendant termination primitive; different access model from arbitrary desktop commands. |
| Custom `sandbox-exec` profile | Potential diagnostic restrictions on operations | Deprecated interface/profile route; not a supported production containment dependency here. |
| EndpointSecurity | Entitled process activity monitoring and selected authorization events | Requires Apple approval, root and FDA; fork is a notification, and monitoring is not an atomic kill-tree API. |

### launchd and privilege separation

The installed SDK's `launchd.plist(5)` describes `AbandonProcessGroup` in terms of remaining processes sharing the job's PGID. A default false value therefore does not establish a larger containment set. Apple also advises launchd-managed jobs not to daemonize or create new sessions; that is a cooperative application convention, not enforcement against arbitrary command payloads. [Apple launchd manual source](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5), [Apple launchd guide](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html).

Local SDK documentation limits `UserName`/`GroupName` to the privileged system domain. A current non-root Guest service cannot create arbitrary per-command credential identities using `setuid`: the permitted unprivileged transitions depend on its existing real/effective/saved credentials. A privileged supervisor must drop supplementary groups, GID and all UID credentials before invoking a command, verify the result, and keep its control process outside the command UID. Merely changing environment variables or effective UID is insufficient. [Apple setuid manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/setuid.2.html), [Apple privilege separation guidance](https://developer.apple.com/library/archive/documentation/Security/Conceptual/SecureCodingGuide/Articles/AccessControl.html).

A system LaunchDaemon installed through `SMAppService` needs local administrator approval before bootstrap. This uses a public service-management path; the investigation did not identify a special restricted entitlement for ordinary credential dropping. It does **not** eliminate signing, distribution, authenticated helper IPC, upgrade or user-approval requirements. No Developer ID identity or approval has been obtained for this proposal. [Apple SMAppService registration](https://developer.apple.com/documentation/servicemanagement/smappservice/register()).

A separate execution UID changes product behavior: it does not inherit the logged-in desktop user's HOME, files, Keychain, GUI session or TCC grants. File upload and GUI automation cannot silently acquire these rights through the command service. Headless command execution and same-console app launching must remain distinct capabilities until an explicit access design is tested. Apple's system-daemon description also distinguishes these daemons from logged-in user GUI agents. [Apple daemon design guide](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/DesigningDaemons.html).

### Why UID cleanup is a candidate, not a guarantee

Apple documents a non-root `kill(-1, signal)` form covering processes with the caller's UID, excluding the caller, and warns through its success semantics that success can mean some process could be signaled. Never invoke this as the desktop service user; never invoke it as root. A dedicated cleanup helper could permanently drop to an exclusively owned command UID before using the primitive. An independent supervisor would still have to verify termination. [Apple kill manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kill.2.html).

Engineering inference: neither that interface nor repeated `proc_listpids(PROC_UID_ONLY/PROC_RUID_ONLY)` scans documents a cgroup-style atomic boundary preventing concurrent process creation. A successful signal, an empty single scan, or a quiet delay is not sufficient proof against adversarial concurrent forks. Processes that change credentials through set-ID executables or delegate work to launch services require additional policy. An IPC request can cause an existing service to act without creating a descendant at all. A UID also grants ordinary OS access and is not a filesystem/network sandbox.

The SDK declares `proc_signal_with_audittoken`/`proc_terminate_with_audittoken`; these deserve an availability/identity test before any production use rather than assuming naked PID enumeration is safe. They do not supply missing membership/admission semantics. Do not use private coalition, launchd or kernel APIs as a release dependency. `NOTE_TRACK`, `NOTE_TRACKERR` and `NOTE_CHILD` are explicitly unsupported since macOS 10.5 in the current `sys/event.h`, so recursive kqueue tracking is not an alternative. [Apple XNU event header](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/event.h), [Apple libproc implementation](https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.c).

### Sandbox and EndpointSecurity limits

The installed `sandbox-exec(1)` labels the utility deprecated and directs developers to App Sandbox. `sandbox.h` marks `sandbox_init` unsupported/deprecated since 10.8. An experiment that denies all process creation could sidestep some descendants while breaking normal shells/build tools; it cannot stand in for the established arbitrary-command product requirement. App Sandbox is supported and can restrict child capabilities, but adopting it would require compatibility tests and a separate helper design: Apple lists several desktop integration operations as incompatible. Its documentation does not promise recursive cancellation. [Apple App Sandbox guide](https://developer.apple.com/documentation/security/protecting-user-data-with-app-sandbox), [Apple inheritance rules](https://developer.apple.com/library/archive/documentation/Miscellaneous/Reference/EntitlementKeyReference/Chapters/EnablingAppSandbox.html).

EndpointSecurity requires an entitlement requested from Apple. The local SDK additionally documents root execution and Full Disk Access/TCC requirements. Its event enum has `AUTH_EXEC` but only `NOTIFY_FORK`, not `AUTH_FORK`. Thus even approved ES access is not by itself a fork-denial or kill-subtree guarantee. Entitlement approval and operational permissions are external dependencies; ordinary Developer Program membership alone should not be represented as approval. No entitlement was requested and SIP was not changed. [Apple entitlement requirements](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.endpoint-security.client), [Apple es_new_client API](https://developer.apple.com/documentation/endpointsecurity/es_new_client(_:_:)).

## Minimal next experiment and acceptance gates

Prepare a dedicated disposable Guest fixture, not the existing interactive VM. Installing a privileged supervisor and execution account(s) is an explicit separate action; this investigation does not perform it. Begin with one job and a unique test UID, plus a second unrelated fixture identity as a kill-isolation sentinel. Do not reuse an existing Guest user's UID. Limit the fixture itself to a fixed small descendant count and watchdog lifetime; do not run an unbounded fork bomb.

1. Use a root-owned minimal supervisor, fixed drop-credentials launcher and private job directory. Do not expose arbitrary root commands, remote UID selection or general filesystem APIs. Pin the real/saved/effective UID/GID evidence and verify no supplementary groups. The command receives no helper control endpoint or signing secrets.
2. Have a known fixture fork, call `setsid`, fork again, close inherited output FDs and attempt a delayed marker write. A second bounded fixture creates descendants concurrently with cancellation. Record only fixture PID/incarnation/UID membership, deadline and marker outcomes; no user command content.
3. Exercise explicit cancel, execution deadline, leader natural exit, MCP disconnect, lease takeover and supervisor death/restart. Test a stopped descendant and an intentionally unresponsive cleanup path. Termination failure must fence job/UID reuse and return `cleanup_unconfirmed`; cancellation acknowledgement must remain distinct from observed completion. A dead supervisor cannot promise immediate cancellation.
4. After cleanup, inspect both effective and real UID membership and unique process identities, ensure all recorded descendants exited, check delayed markers stay absent, and confirm the unrelated sentinel survived. UID reuse requires explicit recovered-clean evidence; never “clean up” an ambiguous recycled PID or shared account. Bounded test success still does not prove adversarial atomicity.
5. Separately test whether the command can start a user launchd job, invoke a set-ID helper or delegate execution outside its identity. A successful escape or unresolved creation race is a **no-go for production arbitrary-command containment**, not a reason to weaken the assertion. If supported public restrictions cannot close these routes without breaking requirements, retain unsupported macOS async execution and record the precise platform/product blocker.

The first experiment can establish whether dedicated-UID cancellation catches the known PGID escape under controlled conditions. Shipping requires a stronger, reviewed membership and revocation argument plus actual protocol tests; this document does not supply one. A restricted cooperative execution mode could be considered separately but must not be labeled fulfillment of production cancellation.

## Reproducible inspection record

Inspected on Apple Silicon macOS 26.4.1 build 25E253. Local SDK root from `xcrun --show-sdk-path`: `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`. Read-only checks used:

```sh
rg -n 'AbandonProcessGroup|UserName|GroupName' /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/share/man/man5/launchd.plist.5
rg -n 'DEPRECATED|sandbox_init' /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/sandbox.h /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/share/man/man1/sandbox-exec.1
rg -n 'NOTE_TRACK|NOTE_CHILD' /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/sys/event.h
rg -n 'proc_listpids|proc_signal_with_audittoken' /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/libproc.h
rg -n 'NOT_PRIVILEGED|NOT_ENTITLED|NOT_PERMITTED|AUTH_EXEC|NOTIFY_FORK' /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/EndpointSecurity/ESTypes.h
```

Official references above were inspected on 2026-10-09; archived manuals are cross-checked where possible against installed SDK headers/man pages. No process-containment experiment was run in this investigation, and no third-party message, entitlement request, purchase or deployment occurred.
