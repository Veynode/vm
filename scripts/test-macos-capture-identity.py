#!/usr/bin/env python3
"""Actual Guest MCP identity -> private bridge -> ScreenCaptureKit wire check.

Requires an existing approved capture listener. Never changes permissions, VM
state or input. Saves metadata and hashes, never captured pixel bodies.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("identity", ROOT / "scripts/test-macos-console-identity.py")
identity_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity_module)


def exact(connection, size, deadline):
    data = bytearray()
    while len(data) < size:
        remaining = deadline - time.monotonic()
        assert remaining > 0, "capture_deadline"
        connection.settimeout(min(8, remaining))
        part = connection.recv(size - len(data))
        if not part:
            raise RuntimeError("unexpected_capture_eof")
        data.extend(part)
    return bytes(data)


def capture(path, identity, mode, report):
    request = {"id": "capture-identity-test", "method": "start", "width": 1280, "height": 720, "fps": 30}
    if mode != "legacy":
        request["expected_console_identity"] = dict(identity)
        if mode == "mismatch":
            request["expected_console_identity"]["security_session_id"] ^= 1
    began = time.monotonic()
    deadline = began + (45 if mode == "invalidation" else 20)
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(8)
        connection.connect(str(path))
        connection.sendall(json.dumps(request).encode() + b"\n")
        started = stop_sent = terminal = False
        last_sequence = last_pts = 0
        try:
            for _ in range(2000):
                assert time.monotonic() < deadline, "capture_deadline"
                if mode != "invalidation" and started and report["frames"] and not stop_sent and time.monotonic() - began >= 3:
                    connection.sendall(b'{"id":"capture-identity-stop","method":"stop"}\n')
                    stop_sent = True
                if not select.select([connection], [], [], 0.1)[0]:
                    continue
                size = struct.unpack(">I", exact(connection, 4, deadline))[0]
                assert 1 <= size <= 33 + 1280 * 720 * 3 // 2, "invalid_capture_record_size"
                record = exact(connection, size, deadline)
                if record[0] == 1:
                    event = json.loads(record[1:])
                    report["events"].append(event)
                    if event["event"] == "started":
                        assert mode != "mismatch" and not started, "unexpected_capture_start"
                        assert event["id"] == request["id"], "capture_start_id"
                        assert (event["width"], event["height"], event["pixel_format"]) == (1280, 720, "nv12-video-range"), "capture_start_geometry"
                        assert event["console_identity"] == identity, "capture_identity_mismatch"
                        assert event["console_binding_mode"] == ("observe_unbound" if mode == "legacy" else "expected_identity_matched"), "capture_binding_mode"
                        assert event["frame_console_identity_scope"] == "this_capture_connection", "capture_identity_scope"
                        assert event["console_transition_atomic"] is False and event["input_authorization"] == "not_evaluated", "capture_claim_overreach"
                        assert event["lock_state"] == "unknown", "capture_lock_claim"
                        started = True
                        if mode == "invalidation":
                            print("Actual bound capture started; awaiting independently requested Guest display sleep", flush=True)
                    elif event["event"] == "error":
                        assert event["id"] == request["id"], "capture_error_id"
                        if mode == "invalidation":
                            assert event["code"] in ("console_session_transition", "console_identity_unavailable", "capture_stopped_with_error"), "unexpected_invalidation_error"
                            assert started and report["frames"] > 0, "invalidation_before_actual_capture"
                        else:
                            assert mode == "mismatch" and event["code"] == "console_identity_mismatch", "unexpected_capture_error"
                    elif event["event"] == "terminated":
                        if mode == "invalidation":
                            assert started and report["frames"] > 0 and not stop_sent, "unexpected_requested_invalidation"
                            assert event["cleanup_basis"] in ("stop_capture_completed", "delegate_reported_stopped"), "invalidation_cleanup_basis"
                            errors = [e for e in report["events"] if e["event"] == "error"]
                            assert len(errors) == 1, "missing_invalidation_error"
                            if event["cleanup_basis"] == "delegate_reported_stopped":
                                assert errors[0]["code"] == "capture_stopped_with_error" and errors[0]["error_domain"] == "com.apple.ScreenCaptureKit.SCStreamErrorDomain", "invalidation_delegate_not_attested"
                        else:
                            assert mode == "mismatch" and report["frames"] == 0 and not started, "unexpected_capture_termination"
                            assert event["cleanup_basis"] == "no_stream_created", "capture_failure_created_stream"
                            assert any(e.get("code") == "console_identity_mismatch" for e in report["events"]), "missing_mismatch_error"
                        assert event["id"] == request["id"] and event["cleanup_confirmed"] is True, "capture_failure_cleanup_unconfirmed"
                        assert event["reason"] == "capture_failed", "capture_terminal_reason"
                        terminal = True
                    elif event["event"] == "stopped":
                        assert stop_sent and event["id"] == "capture-identity-stop", "unexpected_capture_stop"
                        assert event["console_identity_reads"] >= 5, "capture_runtime_identity_checks_missing"
                        terminal = True
                    else:
                        raise AssertionError("unknown_capture_event")
                    if terminal:
                        connection.settimeout(max(0.001, min(8, deadline - time.monotonic())))
                        assert connection.recv(1) == b"", "capture_data_after_terminal"
                        report["cleanup_confirmed"] = True
                        report["elapsed_seconds"] = round(time.monotonic() - began, 3)
                        return report
                else:
                    assert record[0] == 2 and started and not terminal, "frame_without_bound_start"
                    sequence, pts, width, height, y_bytes, uv_bytes = struct.unpack(">QQIIII", record[1:33])
                    assert (width, height, y_bytes, uv_bytes) == (1280, 720, 1280 * 720, 1280 * 720 // 2), "raw_frame_geometry"
                    assert len(record) == 33 + y_bytes + uv_bytes and sequence > last_sequence and (not report["frames"] or pts > last_pts), "raw_frame_layout_or_order"
                    last_sequence, last_pts = sequence, pts
                    report["frames"] += 1
                    if report["frames"] == 1:
                        report["first_frame_sha256"] = hashlib.sha256(record[33:]).hexdigest()
                    # Keep the real session alive for three seconds so the
                    # runtime identity polling runs even on an idle desktop.
            raise AssertionError("capture_record_limit")
        finally:
            if not report.get("cleanup_confirmed"):
                # EOF asks the owned connection to tear down, but without a
                # terminal proof this test must fail, never claim clean stop.
                report["cleanup_confirmed"] = False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "state-dir", "binding", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--id", required=True)
    parser.add_argument("--await-display-invalidation", action="store_true", help="Await independently requested Guest display sleep during a bound stream; does not change display state")
    args = parser.parse_args()
    fd = os.open(args.output, os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_WRONLY, 0o600)
    report = {"passed": False, "scope": "actual Guest capture identity correctness", "input_performed": False,
              "transition_race_excluded": False, "performance_validated": False, "cases": []}
    client = None
    try:
        binding = json.loads(args.binding.read_text())["binding"]
        def verify_bridge():
            response = subprocess.run([str(args.binary.resolve()), "call", "--state-dir", str(args.state_dir.resolve()),
                                       "guest_stream_bridge_binding", json.dumps({"id": args.id, **{k: binding[k] for k in ("worker_instance", "bridge_instance")}})], capture_output=True, timeout=15)
            assert response.returncode == 0 and json.loads(response.stdout)["binding"] == binding, "bridge_binding_changed"
        verify_bridge()
        client = identity_module.ObservingMCP(args.binary.resolve(), args.state_dir.resolve())
        def current_identity():
            reply = client.tool("vm_guest_info", {"id": args.id})
            assert not reply.get("isError"), "guest_info_failed"
            guest = json.loads(reply["content"][0]["text"])["result"]
            assert guest["os"] == "darwin" and guest["console_identity_status"]["identity_eligible"] is True, "eligible_guest_required"
            return guest["console_identity"]
        identity = current_identity()
        report["console_identity"] = identity
        report["binding"] = binding
        for mode in (("invalidation",) if args.await_display_invalidation else ("mismatch", "matched", "legacy")):
            verify_bridge()
            assert current_identity() == identity, "guest_identity_changed"
            case = {"mode": mode, "frames": 0, "events": [], "cleanup_confirmed": False}
            report["cases"].append(case)
            capture(Path(binding["socket_path"]), identity, mode, case)
        if args.await_display_invalidation:
            reply = client.tool("vm_guest_info", {"id": args.id})
            assert not reply.get("isError"), "post_invalidation_info_failed"
            guest = json.loads(reply["content"][0]["text"])["result"]
            status = guest["console_identity_status"]
            assert status["identity_eligible"] is False and status["identity_rejection"] in ("display_inactive", "display_asleep"), "display_invalidation_not_observed"
            assert guest["console_identity"] is None and guest["capabilities"]["screenshot"] is False and guest["capabilities"]["input"] is False, "inactive_guest_capabilities_exposed"
            report["post_invalidation_status"] = status
        else:
            assert current_identity() == identity, "guest_identity_changed_after_capture"
        verify_bridge()
        report["passed"] = True
    except Exception as error:
        import re
        message = str(error)
        report["error"] = message if isinstance(error, AssertionError) and re.fullmatch(r"[a-z][a-z0-9_]{0,79}", message) else type(error).__name__
    finally:
        if client:
            try:
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
    raise SystemExit(main())
