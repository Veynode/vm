# HTTPS MCP shutdown verification

The shutdown slice installs SIGINT/SIGTERM handlers before listening. Either signal closes HTTP admission/listening and retires every registered stdio session, including an initializing or already-retiring session. Retirement closes stdin independently of the response reader and drains abandoned stdout with bounded memory. Sessions remain tracked until the owned MCP child exits or bounded forced cleanup finishes. Closing a request future transfers cleanup to a retained task.

The MCP adapter serializes disconnect-monitor and finalizer cleanup. Its first daemon `session_end` outcome is retained: a second empty outcome cannot overwrite failure evidence. Missing/malformed acknowledgement, failed Guest file cleanup, or an unconfirmed cancellation request makes the child exit nonzero. HTTP DELETE returns502 for unconfirmed cleanup rather than204. Failures from previous DELETE or expiry remain latched for server exit. Use the matching updated `vm-control` executable as the trusted `--control-bin`; arbitrary/older executables do not establish this contract merely by exiting0.

**Proof boundary:** successful shutdown confirms `session_end` closure, successful file cleanup replies when applicable, cancellation-request acknowledgements and MCP child reaping. An `exec_cancel` acknowledgement does not prove asynchronous job termination; that needs separate terminal-job reconciliation. None of this is rollback, a graceful SIGKILL guarantee, or proof of Guest cleanup after a forced child kill.

Each child gets at most1second to close stdin and42seconds to finish; forced termination has a further1second reap wait and is always failure. Server session reconciliation is bounded45seconds and HTTP transport draining46seconds. The positive path does not intentionally wait these limits. SIGTERM/SIGINT share the same path; SIGKILL cannot execute it.

## Reproduce

Run from repository root with Python3 and OpenSSL available. Tests create only disposable local directories, a temporary explicitly trusted certificate, Unix protocol fixtures and owned subprocesses; they do not start a VM or access a Guest.

```sh
cargo test -p vm-control --lib --bin vm-mcp-http --test http_shutdown --target-dir .local/http-shutdown-target -j 2 -- --nocapture
cargo clippy -p vm-control --lib --bin vm-control --bin vm-mcp-http --tests --target-dir .local/http-shutdown-target -j 2 -- -D warnings
```

The integration test invokes `http_shutdown.py` with Cargo's actual built binary paths. It uses real TLS verification, real HTTP, the production `vm-control mcp` process, and real OS signals. Its Unix daemon responses are synthetic so failures can be deterministic. The hung-child case instead uses an explicitly synthetic stdio child. This is not Guest or VM acceptance. If a fixture fails before its HTTP parent exits, its dedicated process group is killed while that parent PID remains owned; no live/user process is targeted.

Verified on macOS26.4.1/Apple Silicon,2026-10-09:

- 41 control-library tests passed, including typed cleanup checks and concurrent first-failure retention with one actual Unix RPC.
- 4 HTTP unit tests passed, including cancellation of initialization and a normal request through the production dispatch function. Each uses a real child which emits700KB after stdin EOF; retirement drains and reaps within the3second assertion.
- 1 integration test passed its15 TLS/subprocess scenarios: both signals with idle/multiple/in-flight sessions; file/input/job/malformed cleanup failures; clean and failed DELETE; failed expiry; and a hung child. Normal two-session SIGTERM took0.129s and SIGINT0.144s in this sample; forced cleanup took42.017s and correctly exited1. These timings are fixture results, not performance guarantees.
- Clippy with warnings denied and whitespace validation passed.

The integration JSON printed by the command includes each case's exit code, duration and child-reaping check, with no tokens, private keys or Guest data. Actual macOS Guest partial-upload cleanup and worker-failure validation are separate parent-task evidence.
