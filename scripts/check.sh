#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PATH="/opt/homebrew/bin:$PATH"
export VEYNODE_PYTHON="${VEYNODE_PYTHON:-$(command -v python3.13 || command -v python3.12 || command -v python3)}"
./scripts/build-native.sh
./native/tools/build-vz-desktop-stream.sh
./native/guest-metal-fix/build.sh
./native/tools/build-vm-vision.sh
./native/tools/build-vm-macos-desktop.sh
cargo fmt --check
cargo build --locked --bins
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
go -C guest/veynode-agent test -race ./...
GOOS=linux GOARCH=arm64 go -C guest/veynode-agent vet ./...
python3 -m unittest discover -s tests -v
npm --prefix browser test
if [[ ! web/node_modules/.package-lock.json -nt web/package-lock.json ]]; then npm --prefix web ci --no-audit --no-fund; fi
npm --prefix web run typecheck
npm --prefix web run build
npm --prefix web test
if [[ "${1:-}" == "--real" ]]; then
  python3 scripts/prepare-linux.py
  python3 scripts/build-linux-fixture.py
  python3 scripts/test-control.py --config .local/run/linux-agent.json --require-guest --output .local/evidence/control-guest-smoke.json
  python3 scripts/test-multivm.py
  python3 scripts/test-control-http.py --config .local/run/linux-agent.json
  python3 guest/kernel/build.py
  python3 scripts/test-containers.py
  if command -v docker >/dev/null; then python3 scripts/test-engine.py; fi
fi
