#!/usr/bin/env python3
"""Real macOS MCP anchor/permission-denial check; never grants permission or starts a VM."""
import argparse
import importlib.util
import json
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("mcp_fixture", ROOT / "scripts/test-control-qemu.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--id", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    # Reserve private evidence before touching the service; never overwrite a prior run.
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    report = {"passed": False, "scope": "real Guest permission denial, not input delivery"}
    client = None
    try:
        client = fixture.MCP(args.binary.resolve(), args.state_dir.resolve(), input=True)
        info = client.tool("vm_guest_info", {"id": args.id})
        assert not info["isError"], info
        guest = json.loads(info["content"][0]["text"])["result"]
        report["guest"] = guest
        assert guest["os"] == "darwin" and guest["machine"].startswith("VirtualMac")
        assert guest["input_policy"]["enabled"] is True
        assert guest["input_policy"]["permission"] == "required", "Permission already granted; refusing denial fixture"
        assert guest["input_policy"]["available"] is False
        reply = client.tool("vm_screenshot", {"id": args.id})
        assert not reply["isError"], reply
        frame = json.loads(reply["content"][1]["text"])
        report["frame"] = frame  # Never persist image bodies.
        assert frame["input_anchor_available"] is True
        assert type(frame["source_generation"]) is int
        assert frame["coordinate_space"] == "guest-display-physical-pixels"
        assert frame["width"] == frame["source_width"] and frame["height"] == frame["source_height"]
        report["input"] = client.tool("vm_input_key", {
            "id": args.id, "keys": ["esc"],
            **{key: frame[key] for key in ("frame_id", "source_generation", "width", "height")},
        })
        # If an operator grants TCC concurrently, this harmless Escape can be delivered;
        # that makes this check fail rather than claiming a permission-denial pass.
        assert report["input"]["isError"] is True
        assert "accessibility_permission_required" in json.dumps(report["input"])
        report["passed"] = True
    except BaseException as error:
        report["error"] = repr(error)
        raise
    finally:
        try:
            if client is not None:
                client.close()
        finally:
            with os.fdopen(fd, "w") as evidence:
                json.dump(report, evidence, indent=2)
                evidence.write("\n")
    print("PASS: real screenshot anchor and explicit Guest Accessibility denial")


if __name__ == "__main__":
    main()
