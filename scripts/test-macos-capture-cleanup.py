#!/usr/bin/env python3
"""Actual Guest capture/disconnect/receipt recovery. Saves no pixel bodies."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("capture_identity", ROOT / "scripts/test-macos-capture-identity.py")
capture_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture_module)
BASES = {"no_stream_created", "stop_capture_completed", "delegate_reported_stopped"}


def receive(connection, deadline):
    length = struct.unpack(">I", capture_module.exact(connection, 4, deadline))[0]
    assert 1 < length <= 33 + 1280 * 720 * 3 // 2, "record_size"
    body = capture_module.exact(connection, length, deadline)
    if body[0] == 1:
        assert length <= 65536, "json_record_size"
        return 1, json.loads(body[1:])
    assert body[0] == 2 and length >= 33, "frame_kind"
    sequence, pts, width, height, y_bytes, uv_bytes = struct.unpack(">QQIIII", body[1:33])
    assert (width, height, y_bytes, uv_bytes) == (1280, 720, 1280 * 720, 1280 * 720 // 2), "frame_geometry"
    assert length == 33 + y_bytes + uv_bytes, "frame_layout"
    return 2, {"sequence": sequence, "pts": pts, "sha256": hashlib.sha256(body[33:]).hexdigest()}


def receipt(event):
    assert event["cleanup_protocol_version"] == 1 and type(event["cleanup_protocol_version"]) is int, "receipt_version"
    assert event["cleanup_retention_ms"] == 120000 and type(event["cleanup_retention_ms"]) is int, "receipt_retention"
    result = {key: event[key] for key in ("service_instance", "capture_instance")}
    assert all(isinstance(value, str) and re.fullmatch("[0-9a-f]{32}", value) for value in result.values()), "receipt_identifiers"
    return result


def connect(path, deadline):
    connection = socket.socket(socket.AF_UNIX)
    connection.settimeout(max(0.001, min(5, deadline - time.monotonic())))
    try:
        connection.connect(str(path))
        return connection
    except Exception:
        connection.close()
        raise


def query(path, ticket, deadline):
    with connect(path, deadline) as connection:
        request = {"id": "cleanup-query", "method": "cleanup_status", **ticket}
        connection.sendall(json.dumps(request).encode() + b"\n")
        kind, response = receive(connection, deadline)
        assert kind == 1, "query_returned_pixels"
        keys = {"event", "id", "protocol_version", "service_instance", "capture_instance", "status", "cleanup_confirmed", "retention_ms"}
        if response.get("status") == "clean":
            keys.add("cleanup_basis")
            assert response["cleanup_basis"] in BASES, "query_basis"
        assert set(response) == keys, "query_fields"
        assert response["event"] == "cleanup_status" and response["id"] == request["id"], "query_identity"
        assert type(response["protocol_version"]) is int and response["protocol_version"] == 1, "query_version"
        assert type(response["retention_ms"]) is int and response["retention_ms"] == 120000, "query_retention"
        assert response["capture_instance"] == ticket["capture_instance"], "query_capture_identity"
        assert response["status"] in {"active", "clean", "failed", "unknown", "service_mismatch"}, "query_status"
        assert response["cleanup_confirmed"] is (response["status"] == "clean"), "query_confirmation"
        if response["status"] == "service_mismatch":
            assert re.fullmatch("[0-9a-f]{32}", response["service_instance"]) and response["service_instance"] != ticket["service_instance"], "query_service_mismatch"
        else:
            assert response["service_instance"] == ticket["service_instance"], "query_service_identity"
        connection.settimeout(max(0.001, min(5, deadline - time.monotonic())))
        assert connection.recv(1) == b"", "query_missing_eof"
        return response


def await_clean(path, ticket, report):
    deadline = time.monotonic() + 8
    report["queries"] = []
    report["transport_retries"] = 0
    while time.monotonic() < deadline:
        try:
            value = query(path, ticket, deadline)
        except (BrokenPipeError, ConnectionResetError, ConnectionRefusedError):
            report["transport_retries"] += 1
            time.sleep(0.05)
            continue
        except RuntimeError as error:
            # The single-client host bridge rejects queued clients while it
            # finishes the prior connection; this is not a Guest cleanup proof.
            if str(error) != "unexpected_capture_eof":
                raise
            report["transport_retries"] += 1
            time.sleep(0.05)
            continue
        report["queries"].append(value)
        if value["status"] == "clean":
            assert value["cleanup_basis"] == "stop_capture_completed", "real_capture_wrong_cleanup_basis"
            report["cleanup_confirmed"] = True
            return value
        assert value["status"] == "active", "capture_not_clean_or_pending"
        time.sleep(0.05)
    raise AssertionError("cleanup_query_deadline")


def capture(path, identity, mode, report):
    deadline = time.monotonic() + 15
    connection = connect(path, deadline)
    report.update(frames=0, events=[], cleanup_confirmed=False, stop_ack_received=False)
    ticket = None
    started = False
    sequence = pts = -1
    try:
        request = {"id": "cleanup-start", "method": "start", "width": 1280, "height": 720, "fps": 30,
                   "expected_console_identity": identity, "cleanup_receipt": True}
        connection.sendall(json.dumps(request).encode() + b"\n")
        for _ in range(300):
            kind, value = receive(connection, deadline)
            if kind == 1:
                report["events"].append(value)
                if value["event"] == "capture_registered":
                    assert ticket is None and value["id"] == request["id"], "unexpected_registration"
                    ticket = receipt(value)
                    report["ticket"] = ticket
                elif value["event"] == "started":
                    assert ticket and not started and value["id"] == request["id"] and receipt(value) == ticket, "invalid_started_receipt"
                    assert value["console_identity"] == identity and value["console_binding_mode"] == "expected_identity_matched", "started_console_identity"
                    assert value["frame_console_identity_scope"] == "this_capture_connection", "frame_binding_scope"
                    assert (value["width"], value["height"], value["pixel_format"]) == (1280, 720, "nv12-video-range"), "started_geometry"
                    started = True
                else:
                    raise AssertionError("unexpected_capture_event")
            else:
                assert started and ticket, "frame_before_verified_started"
                assert value["sequence"] > sequence and value["pts"] > pts, "frame_order"
                sequence, pts = value["sequence"], value["pts"]
                report["frames"] += 1
                if report["frames"] == 1:
                    report["first_frame_sha256"] = value["sha256"]
                if report["frames"] >= 2:
                    break
        assert report["frames"] >= 2 and ticket, "real_frames_required"
        if mode == "disconnect":
            report["host_stop_sent"] = False
            # Intentionally discard the old transport without reading an ACK.
            connection.shutdown(socket.SHUT_RDWR)
        else:
            connection.sendall(b'{"id":"cleanup-stop","method":"stop"}\n')
            report["host_stop_sent"] = True
            for _ in range(100):
                kind, value = receive(connection, deadline)
                if kind == 2:
                    assert value["sequence"] > sequence and value["pts"] > pts, "drain_frame_order"
                    sequence, pts = value["sequence"], value["pts"]
                    report["frames"] += 1
                    continue
                report["events"].append(value)
                assert value["event"] == "stopped" and value["id"] == "cleanup-stop" and receipt(value) == ticket, "normal_stop_receipt"
                connection.settimeout(max(0.001, min(5, deadline - time.monotonic())))
                assert connection.recv(1) == b"", "normal_stop_missing_eof"
                report["stop_ack_received"] = True
                break
            assert report["stop_ack_received"], "normal_stop_missing_ack"
    finally:
        connection.close()
    return ticket


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "state-dir", "binding", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--id", required=True)
    args = parser.parse_args()
    fd = os.open(args.output, os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_WRONLY, 0o600)
    report = {"passed": False, "scope": "actual Guest capture cleanup ledger through verified single-client bridge",
              "active_query_actual_tested": False, "active_query_limit": "host_bridge_single_client",
              "pixel_bodies_saved": False, "input_performed": False, "cases": []}
    client = None
    try:
        binding = json.loads(args.binding.read_text())["binding"]
        def verify():
            result = subprocess.run([str(args.binary.resolve()), "call", "--state-dir", str(args.state_dir.resolve()),
                                     "guest_stream_bridge_binding", json.dumps({"id": args.id, **{key: binding[key] for key in ("worker_instance", "bridge_instance")}})], capture_output=True, timeout=10)
            assert result.returncode == 0 and json.loads(result.stdout)["binding"] == binding, "bridge_provenance_changed"
        verify()
        client = capture_module.identity_module.ObservingMCP(args.binary.resolve(), args.state_dir.resolve())
        def current_identity():
            reply = client.tool("vm_guest_info", {"id": args.id})
            assert not reply.get("isError"), "guest_info_failed"
            guest = json.loads(reply["content"][0]["text"])["result"]
            assert guest["os"] == "darwin" and guest["console_identity_status"]["identity_eligible"] is True, "guest_identity_unavailable"
            return guest["console_identity"]
        identity = current_identity()
        report.update(console_identity=identity, binding=binding)
        previous = None
        for mode in ("disconnect", "normal_stop"):
            verify()
            assert current_identity() == identity, "console_identity_changed"
            case = {"mode": mode, "cleanup_confirmed": False}
            report["cases"].append(case)
            ticket = capture(Path(binding["socket_path"]), identity, mode, case)
            verify()
            clean = await_clean(Path(binding["socket_path"]), ticket, case)
            case["repeat"] = query(Path(binding["socket_path"]), ticket, time.monotonic() + 5)
            assert case["repeat"] == clean, "cleanup_query_not_repeatable"
            if previous:
                assert ticket["service_instance"] == previous["service_instance"] and ticket["capture_instance"] != previous["capture_instance"], "capture_identity_reused_or_service_changed"
            previous = ticket
            verify()
        wrong_capture = {**previous, "capture_instance": "0" * 32}
        wrong_service = {**previous, "service_instance": "0" * 32}
        report["wrong_capture"] = query(Path(binding["socket_path"]), wrong_capture, time.monotonic() + 5)
        report["wrong_service"] = query(Path(binding["socket_path"]), wrong_service, time.monotonic() + 5)
        assert report["wrong_capture"]["status"] == "unknown" and report["wrong_service"]["status"] == "service_mismatch", "foreign_receipt_accepted"
        assert current_identity() == identity, "console_identity_changed_after_test"
        verify()
        report["passed"] = True
    except Exception as error:
        message = str(error)
        report["error"] = message if isinstance(error, AssertionError) and re.fullmatch("[a-z][a-z0-9_]{0,79}", message) else type(error).__name__
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
