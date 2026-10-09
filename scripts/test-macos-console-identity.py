#!/usr/bin/env python3
"""Real observe-only MCP check of macOS info/screenshot console identity.

Requires an already running Guest service and existing Screen Recording approval.
Does not grant permissions, inject input, start a VM or retain image bodies.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("mcp_fixture", ROOT / "scripts/test-control-qemu.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ObservingMCP(fixture.MCP):
    def __init__(self, *args):
        try:
            super().__init__(*args)
        except Exception:
            # super may have spawned the process before initialize failed.
            self.close()
            raise

    def close(self):
        process = getattr(self, "p", None)
        if process is None:
            return
        if process.stdin and not process.stdin.closed:
            try:
                process.stdin.close()
            except BrokenPipeError:
                pass
        try:
            code = process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
            raise RuntimeError("mcp_close_timeout") from None
        if code != 0:
            raise RuntimeError("mcp_nonzero_exit")


def jpeg_dimensions(data):
    """Bounded independent SOF geometry check, not full entropy/pixel validation."""
    assert data[:2] == b"\xff\xd8" and data[-2:] == b"\xff\xd9", "invalid_jpeg_envelope"
    offset, dimensions = 2, None
    for _ in range(256):
        assert offset + 4 <= len(data) and data[offset] == 0xff, "invalid_jpeg_marker"
        while offset < len(data) and data[offset] == 0xff:
            offset += 1
        assert offset < len(data), "truncated_jpeg_marker"
        marker = data[offset]
        offset += 1
        assert marker not in [0, 0xd8, 0xd9] and not 0xd0 <= marker <= 0xd7, "unexpected_jpeg_marker"
        assert offset + 2 <= len(data), "truncated_jpeg_segment"
        length = int.from_bytes(data[offset:offset + 2], "big")
        assert length >= 2 and offset + length <= len(data), "invalid_jpeg_segment_length"
        if marker in [0xc0, 0xc1, 0xc2]:
            assert dimensions is None and length >= 8, "invalid_jpeg_sof"
            precision, height, width, components = struct.unpack(">BHHB", data[offset + 2:offset + 8])
            assert precision == 8 and 1 <= components <= 4 and length == 8 + 3 * components, "invalid_jpeg_sof_layout"
            assert width > 0 and height > 0, "empty_jpeg_dimensions"
            dimensions = (width, height)
        if marker == 0xda:
            assert dimensions is not None and length >= 6, "missing_jpeg_sof"
            return dimensions
        offset += length
    raise AssertionError("jpeg_marker_limit")


def frame_metadata(frame, observer_only=False):
    common = ["frame_id", "width", "height", "display_id", "console_identity",
              "console_identity_verified_at", "desktop_bounds_points", "point_pixel_scale",
              "captured_at_us", "sha256", "byte_length"]
    anchors = ["source_generation", "input_anchor_available"]
    if observer_only:
        assert all(k not in frame for k in anchors), "observer_input_anchor_fields_present"
        return {k: frame[k] for k in common}
    assert all(k in frame for k in anchors), "required_input_anchor_fields_missing"
    assert type(frame["source_generation"]) is int and 1 <= frame["source_generation"] <= 2**53 - 1, "invalid_source_generation"
    assert type(frame["input_anchor_available"]) is bool, "invalid_input_anchor_available"
    return {k: frame[k] for k in common + anchors}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--id", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expect-rejection", choices=["display_asleep", "display_inactive", "console_not_active"], help="Require this exact actual identity refusal; does not change Guest state")
    parser.add_argument("--observer-only", action="store_true", help="Require disabled Guest input and absence of screenshot input-anchor fields; default retains the original anchor-field contract")
    args = parser.parse_args()
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    report = {"passed": False, "scope": "actual Guest info and captured-image identity",
              "input_performed": False, "transition_race_excluded": False,
              "browser_binding_validated": False, "expected_rejection": args.expect_rejection,
              "screenshot_contract": "observer_only_no_input_anchor" if args.observer_only else "input_anchor_fields_required"}
    client = None
    try:
        report["controller_sha256"] = hashlib.sha256(args.binary.read_bytes()).hexdigest()
        client = ObservingMCP(args.binary.resolve(), args.state_dir.resolve())

        def info():
            reply = client.tool("vm_guest_info", {"id": args.id})
            assert not reply.get("isError"), "guest_info_failed"
            guest = json.loads(reply["content"][0]["text"])["result"]
            assert guest["os"] == "darwin" and guest["machine"].startswith("VirtualMac"), "not_macos_guest"
            if args.observer_only:
                assert guest["capabilities"].get("input") is False, "observer_input_capability_enabled"
                assert guest["input_policy"].get("enabled") is False and guest["input_policy"].get("available") is False, "observer_input_policy_enabled"
            status = guest["console_identity_status"]
            report.setdefault("observed_identity_status", []).append(status)
            assert status["identity_eligible"] is (not args.expect_rejection), "unexpected_identity_eligibility"
            if args.expect_rejection:
                assert status["identity_rejection"] == args.expect_rejection, "identity_rejection_mismatch"
                assert guest["console_identity"] is None, "ineligible_identity_published"
                assert guest["capabilities"]["screenshot"] is False and guest["capabilities"]["input"] is False, "ineligible_capabilities_published"
            assert status["probe_grants_input"] is False, "identity_must_not_grant_input"
            assert status["lock_state"] == "unknown", "lock_state_must_remain_unknown"
            return {k: guest[k] for k in ["console_identity", "console_identity_status", "capabilities", "input_policy"]}

        before = info()
        report["before"] = before
        reply = client.tool("vm_screenshot", {"id": args.id})
        if args.expect_rejection:
            assert reply.get("isError") is True, "ineligible_screenshot_not_rejected"
            assert all(item.get("type") != "image" for item in reply["content"]), "ineligible_image_leaked"
            assert "console_identity_unavailable" in json.dumps(reply), "unexpected_screenshot_rejection"
            report["screenshot_rejected_without_image"] = True
            report["after"] = info()
            report["passed"] = True
        else:
            assert not reply.get("isError"), "screenshot_failed"
            images = [item for item in reply["content"] if item.get("type") == "image"]
            texts = [item for item in reply["content"] if item.get("type") == "text"]
            assert len(images) == len(texts) == 1, "unexpected_screenshot_content"
            frame = json.loads(texts[0]["text"])
            assert len(images[0]["data"]) <= 12_000_000, "oversized_image"
            jpeg = base64.b64decode(images[0]["data"], validate=True)
            assert images[0]["mimeType"] == "image/jpeg" and jpeg.startswith(b"\xff\xd8\xff"), "unexpected_image_encoding"
            assert len(jpeg) == frame["byte_length"], "image_length_mismatch"
            assert hashlib.sha256(jpeg).hexdigest() == frame["sha256"], "image_hash_mismatch"
            assert jpeg_dimensions(jpeg) == (frame["width"], frame["height"]), "jpeg_encoded_dimensions_mismatch"
            identity = frame["console_identity"]
            assert identity == before["console_identity"], "info_capture_identity_mismatch"
            assert identity["physical_width"] == frame["width"] == frame["source_width"], "width_mismatch"
            assert identity["physical_height"] == frame["height"] == frame["source_height"], "height_mismatch"
            assert identity["display_id"] == frame["display_id"], "display_mismatch"
            bounds = frame["desktop_bounds_points"]
            values = [float(bounds[k]) for k in ["x", "y", "width", "height"]]
            values = [0.0 if v == 0 else v for v in values]
            canonical = (b"Veynode/GuestDisplay/v1\0" + struct.pack(">III", frame["display_id"], frame["width"], frame["height"])
                         + struct.pack(">dddd", *values))
            assert hashlib.sha256(canonical).hexdigest() == identity["display_geometry_sha256"], "independent_geometry_hash_mismatch"
            assert frame["console_identity_verified_at"] == "capture_pre_enumeration_and_post_encode", "capture_checks_missing"
            report["frame"] = frame_metadata(frame, args.observer_only)
            after = info()
            report["after"] = after
            assert after["console_identity"] == identity, "post_capture_identity_changed"
            report["passed"] = True
    except Exception as error:
        # No arbitrary Guest message, username or image payload enters evidence.
        # Shared MCP assertions may contain an entire reply. Only our bounded
        # constant codes are eligible for evidence; never serialize that payload.
        message = str(error)
        report["error"] = message if isinstance(error, AssertionError) and re.fullmatch(r"[a-z][a-z0-9_]{0,79}", message) else type(error).__name__
    finally:
        try:
            if client is not None:
                client.close()
        except Exception:
            report["passed"] = False
            report["cleanup_error"] = "mcp_close_failed"
        with os.fdopen(fd, "w") as output:
            json.dump(report, output, indent=2)
            output.write("\n")
    print(json.dumps({"passed": report["passed"], "output": str(args.output.resolve())}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
