# Low-level control path

```sh
./target/debug/vm-control daemon --state-dir /absolute/private/state --backend "$PWD/.local/bin/vm-apple"
./target/debug/vm-control call --state-dir /absolute/private/state create "$(cat .local/run/linux-agent.json)"
./target/debug/vm-control call --state-dir /absolute/private/state start '{"id":"linux-agent"}'
./target/debug/vm-control mcp  --state-dir /absolute/private/state --allow-lifecycle --allow-execute
```

The last command is a stdio MCP server. Observation is on by default; lifecycle, execute, input and files are separate opt-ins. `native/tools/build-vm-vision.sh` builds the host text recognizer behind `vm_screen_text`; rerun `scripts/install-dev-runtime.py` to register it. Read the [control usage and HTTPS security boundaries](../../crates/control/README.md) before exposing any listener. For QEMU/GPU registration flags, see the same document.

[Native backend](../../native/README.md) · [Security](../security.md) · [Linux graphics decision](../decisions/0002-linux-graphics.md) · [Decisions](../decisions/)
