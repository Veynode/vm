#!/usr/bin/env python3
"""Read-only public session identity test; never captures pixels or posts input."""
import argparse
import concurrent.futures
import hashlib
import json
import os
import platform
from pathlib import Path
import struct
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # Preserve both passed and failed reports. Never replace existing evidence.
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    report = {"passed": False, "scope": "host-read-only-public-session-metadata",
              "platform": platform.system(), "architecture": platform.machine(),
              "capture_performed": False, "input_performed": False,
              "guest_validation": False, "transition_validation": False}

    def run(mode):
        # Each call starts a separate executable; no in-process cached identity.
        child = subprocess.run([str(args.probe.resolve()), mode], capture_output=True, timeout=10, check=False)
        if child.returncode != 0 or len(child.stdout) > 16_384 or len(child.stderr) > 1_024:
            raise RuntimeError("probe_process_failed_or_oversize")
        return json.loads(child.stdout)

    try:
        report["probe_sha256"] = hashlib.sha256(args.probe.read_bytes()).hexdigest()
        tests = run("--self-test")
        assert tests["passed"] is True, "pure_tests_failed"
        # Independent canonical vector implementation, including negative origin.
        canonical = (b"Veynode/GuestDisplay/v1\0" + struct.pack(">III", 1, 2560, 1440)
                     + struct.pack(">dddd", -1280.0, 0.0, 1280.0, 720.0))
        assert tests["geometry_vector_sha256"] == hashlib.sha256(canonical).hexdigest(), "canonical_vector_mismatch"
        report["pure_tests"] = tests
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(run, "--read-identity") for _ in range(2)]
            observations = [f.result() for f in futures]
        report["independent_child_observations"] = observations
        for observation in observations:
            assert observation["identity_eligible"] is True, "actual_identity_unavailable"
            assert observation["probe_grants_input"] is False, "probe_must_not_authorize_input"
            assert observation["input_authorization"] == "not_evaluated", "probe_not_input_policy"
            assert observation["lock_state"] == "unknown", "no_lock_predicate_claim"
        assert observations[0]["identity"] == observations[1]["identity"], "independent_session_identity_mismatch"
        report["same_session_two_processes_match"] = True
        report["passed"] = True
    except Exception as error:
        # Do not save subprocess stderr or arbitrary OS session dictionary/name.
        report["error"] = str(error) if isinstance(error, (AssertionError, RuntimeError)) else type(error).__name__
    finally:
        with os.fdopen(fd, "w") as output:
            json.dump(report, output, indent=2, sort_keys=True)
            output.write("\n")
    print(json.dumps({"passed": report["passed"], "output": str(args.output.resolve())}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
