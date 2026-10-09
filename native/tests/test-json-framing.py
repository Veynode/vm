#!/usr/bin/env python3
"""Compile the actual native worker's framing path; never start a VM or GUI."""
import base64
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
with tempfile.TemporaryDirectory(prefix="veynode-json-framing-") as temporary:
    binary = Path(temporary) / "framing-test"
    subprocess.run([
        "xcrun", "swiftc", "-swift-version", "5", "-O", "-D", "JSON_FRAMING_TESTS",
        "-target", "arm64-apple-macosx14.0", "-framework", "Virtualization", "-framework", "AppKit",
        str(ROOT / "native/Sources/main.swift"),
        str(ROOT / "native/Sources/GuestStreamBridge.swift"),
        str(ROOT / "native/tests/json-framing-tests.swift"), "-o", str(binary),
    ], check=True, timeout=120)
    completed = subprocess.run([str(binary), "framing-test"], check=True,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
    line = completed.stdout
    assert line.endswith(b"\n") and line.count(b"\n") == 1
    assert len(line) < 65536, "production emit expanded bounded base64"
    result = json.loads(line)
    assert result["event"] == "framing-test"
    assert result["payload_bytes"] == 32768
    assert base64.b64decode(result["data"], validate=True) == b"\xff" * 32768
    assert result["request_line_bytes"] < 65536
    print(json.dumps({"passed": True, "request_line_bytes": result["request_line_bytes"],
                      "emit_line_bytes": len(line), "payload_bytes": 32768,
                      "boundaries": "65535 JSON accepted; 65536 JSON rejected",
                      "scope": "Actual host request encoder and stdout pipe; no VM acceptance"}))
