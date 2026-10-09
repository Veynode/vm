#!/usr/bin/env python3
"""Actual running macOS VM bridge listener identity test (no capture or input).

Uses a new private fixture directory and stops every bridge it creates. Does not
start/stop a VM. Run again after an independently coordinated worker restart with
--prior-report to verify actual prior-instance rejection.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--binary", type=Path, required=True)
    p.add_argument("--state-dir", type=Path, required=True)
    p.add_argument("--id", required=True)
    p.add_argument("--fixture-dir", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--prior-report", type=Path)
    args = p.parse_args()
    output_fd = os.open(args.output, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    report = {"passed": False, "scope": "actual host worker/listener provenance only",
              "guest_capture_validated": False, "input_authorized": False, "checks": []}
    bridge_started = False
    start_outcome_unknown = False

    def call(method, data, success=True):
        result = subprocess.run([str(args.binary.resolve()), "call", "--state-dir", str(args.state_dir.resolve()), method, json.dumps(data)], capture_output=True, text=True, timeout=15)
        assert (result.returncode == 0) is success, f"unexpected_{method}_outcome"
        if not success:
            assert result.stderr.startswith("vm-control:"), "expected_control_error"
            return {"rejected": True, "message": result.stderr.strip()[:512]}
        return json.loads(result.stdout)

    def query(binding, success=True):
        return call("guest_stream_bridge_binding", {"id": args.id, **{k: binding[k] for k in ["worker_instance", "bridge_instance"]}}, success)

    def start(path):
        nonlocal bridge_started, start_outcome_unknown
        start_outcome_unknown = True
        value = call("start_guest_stream_bridge", {"id": args.id, "socket_path": str(path)})
        start_outcome_unknown = False
        bridge_started = True  # Only a successful command establishes cleanup ownership.
        assert value["listening"] is True and value["guest_connected"] is False
        binding = value["binding"]
        assert binding["scope"] == "host-worker-listener-only" and binding["input_authorized"] is False
        assert binding["vm_id"] == args.id and binding["socket_path"] == str(path)
        info = path.lstat()
        assert stat.S_ISSOCK(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o600
        assert binding["socket_device"] == info.st_dev and binding["socket_inode"] == info.st_ino
        assert binding["worker_pid"] == report["worker_pid"]
        return binding

    def stop():
        nonlocal bridge_started
        reply = call("stop_guest_stream_bridge", {"id": args.id})
        assert reply["stopped"] is True
        bridge_started = False

    try:
        fixture = args.fixture_dir.absolute()
        fixture.mkdir(mode=0o700)  # Refuse all previous/user fixture contents.
        assert len(os.fsencode(fixture / "stream.sock")) < 104, "socket_path_too_long"
        report["binary_sha256"] = hashlib.sha256(args.binary.read_bytes()).hexdigest()
        current = call("status", {"id": args.id})
        assert current["state"] == "running" and current["config"]["guest"] == "macos", "running_macos_required"
        report["worker_pid"] = current["pid"]
        if args.prior_report:
            previous = json.loads(args.prior_report.read_text())
            assert previous["passed"] is True, "prior_report_failed"
            assert previous["worker_pid"] != current["pid"], "worker_did_not_restart"
            prior = previous["first_binding"]
            report["prior_worker_rejection"] = query(prior, False)
        path = fixture / "stream.sock"
        first = start(path)
        report["first_binding"] = first
        assert query(first)["binding"] == first, "initial_query_mismatch"
        report["checks"].append("actual_worker_start_and_query")
        wrong = {**first, "worker_instance": "0" * 64 if first["worker_instance"] != "0" * 64 else "1" * 64}
        report["wrong_worker_rejection"] = query(wrong, False)
        report["duplicate_start_rejection"] = call("start_guest_stream_bridge", {"id": args.id, "socket_path": str(path)}, False)
        assert query(first)["binding"] == first, "duplicate_start_corrupted_binding"
        stop()
        assert not path.exists(), "stop_did_not_remove_own_socket"
        report["stopped_rejection"] = query(first, False)
        second = start(path)
        assert second["worker_instance"] == first["worker_instance"] and second["bridge_instance"] != first["bridge_instance"], "bridge_restart_identity_invalid"
        report["old_bridge_rejection"] = query(first, False)
        assert query(second)["binding"] == second, "restart_query_mismatch"
        if args.prior_report:
            assert second["worker_instance"] != prior["worker_instance"], "restarted_worker_instance_reused"
            report["prior_worker_with_new_bridge_rejection"] = query({**second, "worker_instance": prior["worker_instance"]}, False)
        report["checks"].append("stop_and_same_path_restart_fence")
        # Deliberately replace only this test's own listener pathname. No client
        # has connected. Stop must preserve a newly created unrelated file.
        before = path.lstat()
        assert before.st_ino == second["socket_inode"] and before.st_dev == second["socket_device"]
        path.unlink()
        marker = b"owned-provenance-test-replacement\n"
        fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as replacement:
            replacement.write(marker)
        report["replacement_rejection"] = query(second, False)
        stop()
        assert path.read_bytes() == marker, "stop_removed_unrelated_replacement"
        report["checks"].append("replacement_rejected_and_preserved")
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error) if isinstance(error, AssertionError) else type(error).__name__
    finally:
        if start_outcome_unknown:
            # A busy/refused start may belong to another caller. An unknown ACK
            # may have created ours. Neither authorizes blindly stopping a VM's
            # current bridge; preserve the failed report for explicit inspection.
            report["passed"] = False
            report["start_outcome_unconfirmed_no_cleanup_authority"] = True
        if bridge_started:
            try:
                stop()
            except Exception:
                report["passed"] = False
                report["cleanup_error"] = "bridge_stop_unconfirmed"
        with os.fdopen(output_fd, "w") as evidence:
            json.dump(report, evidence, indent=2)
            evidence.write("\n")
    print(json.dumps({"passed": report["passed"], "output": str(args.output.resolve())}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
