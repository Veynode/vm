#!/usr/bin/env python3
"""Bounded real ENOSPC on a newly created 256 MiB APFS image, never the host volume.

No VM is started. This checks host image publication and daemon registry durability,
not a Guest filesystem's handling of disk exhaustion or sudden power loss.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
LIMIT = 256 * 1024 * 1024


def sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "archive/docs-evidence/control-enospc.json")
    args = parser.parse_args()
    result = {"started_at_unix": time.time(), "checks": [], "image_capacity_bytes": LIMIT,
              "scope": "Host APFS image publication and registry; no Guest, power-loss or crash-consistency acceptance"}
    tmp = Path(tempfile.mkdtemp(prefix="vm-enospc-", dir="/tmp")).resolve()
    image, mount = tmp / "fault.dmg", tmp / "mnt"
    device, daemon, log = None, None, None
    safe_to_remove = True

    def record(name, evidence):
        result["checks"].append({"name": name, "evidence": evidence})

    def run(command, timeout=30):
        return subprocess.run([str(item) for item in command], capture_output=True, text=True, timeout=timeout)

    def require(command, timeout=30):
        reply = run(command, timeout)
        assert reply.returncode == 0, reply.stderr or reply.stdout
        return reply.stdout

    def control(method, value=None):
        return run([ROOT / "target/debug/vm-control", "call", "--state-dir", mount / "state",
                    method, json.dumps(value or {})])

    def start_daemon():
        nonlocal daemon
        process = subprocess.Popen([str(ROOT / "target/debug/vm-control"), "daemon", "--state-dir",
                                    str(mount / "state"), "--backend", str(ROOT / ".local/bin/vm-apple")],
                                   stdout=log, stderr=log)
        daemon = process
        for _ in range(100):
            if process.poll() is not None:
                raise AssertionError("daemon exited during startup; see daemon log")
            if (mount / "state/control.sock").exists():
                return process
            time.sleep(0.05)
        raise AssertionError("daemon socket timeout")

    def image_command(*arguments):
        return run([ROOT / "target/debug/vm-image", *arguments, "--store", mount / "store",
                    "--min-free-mib", "0"])

    try:
        assert not image.exists() and not mount.exists(), "refuse pre-existing target"
        assert shutil.disk_usage(tmp).free > 1024 * 1024 * 1024, "need 1 GiB host headroom"
        require(["hdiutil", "create", "-size", "256m", "-fs", "APFS", "-type", "UDIF",
                 "-nospotlight", "-volname", "VeynodeENOSPC", image], 60)
        safe_to_remove = False
        attached = run(["hdiutil", "attach", "-nobrowse", "-mountpoint", mount, "-plist", image], 60)
        assert attached.returncode == 0, attached.stderr
        safe_to_remove = False
        entities = plistlib.loads(attached.stdout.encode())["system-entities"]
        device = next(item["dev-entry"] for item in entities if item.get("content-hint") == "GUID_partition_scheme")
        volume = next(item for item in entities if item.get("mount-point") == str(mount))
        assert volume["volume-kind"] == "apfs"
        assert mount.is_mount() and mount.stat().st_dev != tmp.stat().st_dev
        capacity = os.statvfs(mount).f_blocks * os.statvfs(mount).f_frsize
        assert 0 < capacity <= LIMIT, "refuse filling a volume larger than this disposable image"
        record("new bounded APFS disk image attached", {"disk_image": str(image), "device": device,
               "volume": volume, "filesystem_capacity_bytes": capacity})

        source = mount / "source.raw"
        with source.open("xb") as output:
            for _ in range(8):
                output.write(os.urandom(1024 * 1024))
            output.flush()
            os.fsync(output.fileno())
        base_reply = image_command("import", "--id", "base", "--source", source, "--source-stopped")
        assert base_reply.returncode == 0, base_reply.stderr
        base = Path(json.loads(base_reply.stdout)["disk_path"])
        manifest = base.parent / "manifest.json"
        original = {"source": sha(source), "base": sha(base), "manifest": sha(manifest)}
        assert original["source"] == original["base"] == json.loads(manifest.read_text())["initial_sha256"]
        # Create all parent paths before exhaustion so the product's publication is exercised.
        good_clone = image_command("clone", "--base", "base", "--id", "existing")
        assert good_clone.returncode == 0, good_clone.stderr
        existing_clone = Path(json.loads(good_clone.stdout)["disk_path"])

        log = (tmp / "daemon.log").open("wb")
        daemon = start_daemon()
        for number in range(8):
            config = {"id": f"existing-{number}", "guest": "linux", "kernel": str(source),
                      "cpus": 1, "memory_mib": 256, "command_line": "x" * 16384}
            reply = control("create", config)
            assert reply.returncode == 0, reply.stderr
        registry = mount / "state/registry.json"
        registry_bytes = registry.read_bytes()
        registry_hash = sha(registry)
        expected_ids = sorted(json.loads(registry_bytes))
        record("existing image and registry initialized", {"hashes": original, "registry_sha256": registry_hash,
               "registry_bytes": len(registry_bytes), "registered_vm_ids": expected_ids,
               "note": "Configurations only; disposable source is not a bootable kernel and no VM is started"})

        filler = mount / "fill.bin"
        written = 0
        errors = []
        with filler.open("xb", buffering=0) as output:
            for chunk_size in (1024 * 1024, 4096):
                chunk = os.urandom(chunk_size)
                while written <= LIMIT:
                    try:
                        count = output.write(chunk)
                        assert count is not None and count > 0
                        written += count
                        os.fsync(output.fileno())
                    except OSError as error:
                        assert error.errno == errno.ENOSPC, repr(error)
                        errors.append({"errno": error.errno, "message": str(error), "chunk_bytes": chunk_size})
                        break
                else:
                    raise AssertionError("bounded fill limit exceeded; refusing further writes")
        assert len(errors) == 2, "did not observe real ENOSPC"
        record("filesystem returned real ENOSPC", {"successful_write_bytes": written, "errors": errors,
               "free_bytes": os.statvfs(mount).f_bavail * os.statvfs(mount).f_frsize})

        failed_clone = image_command("clone", "--base", "base", "--id", "full-clone")
        assert failed_clone.returncode != 0, "clone unexpectedly succeeded on full filesystem"
        assert "No space left on device" in failed_clone.stderr and "28" in failed_clone.stderr, failed_clone.stderr
        assert not (mount / "store/clones/full-clone").exists()
        assert not list((mount / "store/clones").glob(".staging-*"))
        assert sha(base) == original["base"] and sha(manifest) == original["manifest"]
        assert sha(source) == original["source"] and sha(existing_clone) == original["base"]
        record("image ENOSPC preserves base and existing clone; no partial publication", failed_clone.stderr.strip())

        failed_create = control("create", {"id": "full-registry", "guest": "linux", "kernel": str(source),
                                          "cpus": 1, "memory_mib": 256, "command_line": "y" * 16384})
        assert failed_create.returncode != 0, "registry unexpectedly updated on full filesystem"
        assert "No space left on device" in failed_create.stderr, failed_create.stderr
        assert registry.read_bytes() == registry_bytes and sha(registry) == registry_hash
        assert not list(registry.parent.glob("*.tmp"))
        listing = control("list")
        assert listing.returncode == 0, listing.stderr
        assert sorted(item["config"]["id"] for item in json.loads(listing.stdout)) == expected_ids
        record("registry ENOSPC preserves durable and in-memory state", failed_create.stderr.strip())

        filler.unlink()
        recovered = control("create", {"id": "recovered", "guest": "linux", "kernel": str(source),
                                       "cpus": 1, "memory_mib": 256})
        assert recovered.returncode == 0, recovered.stderr
        daemon.terminate()
        daemon.wait(timeout=10)
        (mount / "state/control.sock").unlink(missing_ok=True)
        daemon = start_daemon()
        listing = control("list")
        assert listing.returncode == 0, listing.stderr
        assert sorted(item["config"]["id"] for item in json.loads(listing.stdout)) == sorted(expected_ids + ["recovered"])
        recovered_clone = image_command("clone", "--base", "base", "--id", "recovered")
        assert recovered_clone.returncode == 0, recovered_clone.stderr
        assert sha(Path(json.loads(recovered_clone.stdout)["disk_path"])) == original["base"]
        record("writes resume after freeing space and registry survives daemon restart", True)
        result["passed"] = True
    except Exception as error:
        result["passed"] = False
        result["error"] = str(error)
    finally:
        if daemon and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=10)
        if log:
            log.close()
        if (tmp / "daemon.log").exists():
            result["daemon_log"] = (tmp / "daemon.log").read_text()[-8192:]
        if device:
            detached = run(["hdiutil", "detach", device], 30)
            safe_to_remove = detached.returncode == 0 and not mount.is_mount()
            result["cleanup"] = {"detached": safe_to_remove, "stdout": detached.stdout.strip(), "stderr": detached.stderr.strip()}
            if not safe_to_remove:
                result["passed"] = False
                result["cleanup"]["preserved_path"] = str(tmp)
        if safe_to_remove and not mount.is_mount():
            shutil.rmtree(tmp)
        else:
            result["preserved_path"] = str(tmp)
        result["finished_at_unix"] = time.time()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        output = args.output if result["passed"] else args.output.with_suffix(".failed.json")
        output.write_text(json.dumps(result, indent=2) + "\n")
        print(f"{'PASS' if result['passed'] else 'FAIL'}: {output}")
    if not result["passed"]:
        raise SystemExit(result.get("error", "cleanup failed"))


if __name__ == "__main__":
    main()
