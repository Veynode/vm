#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PATH="/opt/homebrew/bin:$PATH"
./scripts/build-native.sh
cargo fmt --check
cargo build --locked --bins
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
go -C guest/veynode-agent test -race ./...
python3 -m unittest discover -s tests -v
if [[ "${1:-}" == "--real" ]]; then
  python3 scripts/prepare-linux.py
  python3 scripts/build-linux-fixture.py
  python3 scripts/test-control.py --config .local/run/linux-agent.json --require-guest --output .local/evidence/control-guest-smoke.json
  python3 scripts/test-multivm.py
  python3 scripts/test-control-http.py --config .local/run/linux-agent.json
fi
