#!/usr/bin/env python3
"""Real ENOSPC on a self-created bounded APFS image; never fills the host volume."""
import argparse
import base64
import errno
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import select
import shutil
import subprocess
import tempfile
import time

CAPACITY = 256 * 1024 * 1024
OWNER = "a" * 64

def command(argv, timeout=45):
    result = subprocess.run(argv, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError("command_failed:" + Path(argv[0]).name + ":" + result.stderr.decode(errors="replace")[:400])
    return result.stdout

class Worker:
    def __init__(self, binary, root):
        self.child = subprocess.Popen([str(binary), "--file-worker", "--files-root", str(root)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        os.set_blocking(self.child.stdin.fileno(), False)
        self.number = 0
        self.buffer = bytearray()
    def call(self, method, **args):
        self.number += 1
        request = {"id": str(self.number), "method": method, **args}
        if method != "file_worker_info":
            request["owner"] = OWNER
        wire = json.dumps(request).encode() + b"\n"
        deadline = time.monotonic() + 7
        offset = 0
        while offset < len(wire):
            left = deadline - time.monotonic()
            assert left > 0 and select.select([], [self.child.stdin], [], left)[1], "worker_write_timeout"
            try:
                written = os.write(self.child.stdin.fileno(), wire[offset:])
            except BlockingIOError:
                continue
            assert written > 0, "worker_write_failed"
            offset += written
        while b"\n" not in self.buffer:
            left = deadline - time.monotonic()
            assert left > 0 and select.select([self.child.stdout], [], [], left)[0], "worker_response_timeout"
            chunk = os.read(self.child.stdout.fileno(), 65536)
            assert chunk, "worker_eof"
            self.buffer.extend(chunk)
            assert len(self.buffer) <= 65536, "worker_response_size"
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        reply = json.loads(line)
        assert reply["id"] == request["id"], "worker_response_identity"
        return reply
    def close(self):
        self.child.stdin.close()
        try:
            self.child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait(timeout=3)
            raise AssertionError("worker_cleanup_forced")
        self.child.stdout.close()
        assert self.child.returncode == 0, "worker_cleanup_failed"

def good(reply):
    assert "error" not in reply and isinstance(reply.get("result"), dict), reply
    return reply["result"]

def sha(data):
    return hashlib.sha256(data).hexdigest()

def upload(worker, name, body):
    token = good(worker.call("file_upload_begin", path=name, size=len(body), sha256=sha(body)))["token"]
    for offset in range(0, len(body), 32768):
        good(worker.call("file_upload_write", token=token, offset=offset, data=base64.b64encode(body[offset:offset+32768]).decode()))
    return good(worker.call("file_upload_commit", token=token))

def attached_image(image):
    images = plistlib.loads(command(["/usr/bin/hdiutil", "info", "-plist"]))["images"]
    return [item for item in images if Path(item.get("image-path", "/")).resolve() == image]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    scratch = Path(tempfile.mkdtemp(prefix="files-enospc-", dir=Path(__file__).resolve().parents[2] / ".local")).resolve()
    image = scratch / "bounded.dmg"
    mount = scratch / "mount"
    mount.mkdir(mode=0o700)
    device = None
    worker = None
    report = {"passed": False, "scope": "actual host APFS disk-image file-worker ENOSPC; no Guest acceptance", "image_capacity_bytes": CAPACITY, "binary_sha256": sha(binary.read_bytes()), "cleanup": {}, "pixel_or_user_files_read": False}
    try:
        command(["/usr/bin/hdiutil", "create", "-size", "256m", "-fs", "APFS", "-volname", "VeynodeFilesENOSPC", "-type", "UDIF", "-nospotlight", str(image)])
        attached = plistlib.loads(command(["/usr/bin/hdiutil", "attach", "-owners", "on", "-nobrowse", "-mountpoint", str(mount), "-plist", str(image)]))
        entries = attached["system-entities"]
        devices = [entry["dev-entry"] for entry in entries if re.fullmatch(r"/dev/disk\d+", entry.get("dev-entry", ""))]
        assert len(devices) >= 1, "missing_image_device"
        device = devices[0]
        volumes = [entry for entry in entries if entry.get("mount-point") == str(mount)]
        assert len(volumes) == 1, "image_mount_mismatch"
        owners = attached_image(image)
        assert len(owners) == 1 and volumes[0]["dev-entry"] in [e.get("dev-entry") for e in owners[0]["system-entities"]], "image_device_provenance"
        info = plistlib.loads(command(["/usr/sbin/diskutil", "info", "-plist", str(mount)]))
        assert info["MountPoint"] == str(mount) and info["FilesystemType"] == "apfs", "wrong_filesystem"
        assert info["DeviceNode"] == volumes[0]["dev-entry"], "wrong_mount_device"
        assert mount.stat().st_dev != scratch.stat().st_dev, "host_volume_refused"
        stats = os.statvfs(mount)
        total = stats.f_blocks * stats.f_frsize
        assert 0 < total <= CAPACITY and image.stat().st_size <= CAPACITY + 1024 * 1024, "unbounded_test_volume_refused"
        report["verified_volume_bytes"] = total
        report["filesystem"] = "apfs"
        root = mount / "files"
        root.mkdir(mode=0o700)
        worker = Worker(binary, root)
        policy = good(worker.call("file_worker_info"))
        assert policy["uid"] == os.getuid() != 0 and policy["max_file_bytes"] == 8388608
        sentinel = os.urandom(8192)
        assert upload(worker, "published.bin", sentinel)["published"] is True
        pending = bytes([0x5b]) * 8388608
        token = good(worker.call("file_upload_begin", path="pending.bin", size=len(pending), sha256=sha(pending)))["token"]
        good(worker.call("file_upload_write", token=token, offset=0, data=base64.b64encode(pending[:32768]).decode()))
        staged = root / ".veynode-staging" / (".veynode-upload-" + token + ".part")
        assert staged.stat().st_size == 32768, "partial_upload_missing"
        filler = mount / "bounded-filler.bin"
        count = 0
        fill_errno = None
        with filler.open("xb", buffering=0) as stream:
            block = os.urandom(32768)
            while count <= CAPACITY:
                try:
                    n = stream.write(block)
                    assert n and n > 0, "filler_short_zero_write"
                    count += n
                except OSError as error:
                    assert error.errno == errno.ENOSPC, "unexpected_filler_error"
                    fill_errno = error.errno
                    break
            assert fill_errno == errno.ENOSPC, "bounded_volume_did_not_fill"
            try:
                os.fsync(stream.fileno())
            except OSError as error:
                assert error.errno == errno.ENOSPC, "unexpected_filler_sync_error"
        report["filler_bytes_written"] = count
        report["filler_errno"] = fill_errno
        report["free_bytes_at_failure"] = os.statvfs(mount).f_bavail * stats.f_frsize
        offset = 32768
        failure = None
        while offset < len(pending):
            reply = worker.call("file_upload_write", token=token, offset=offset, data=base64.b64encode(pending[offset:offset+32768]).decode())
            if "error" in reply:
                failure = reply["error"]
                break
            assert good(reply)["offset"] == offset + 32768
            offset += 32768
        assert failure and failure["code"] == "file_error" and "errno 28" in failure["message"], "actual_worker_ENOSPC_required"
        report["worker_error"] = failure
        report["upload_bytes_accepted_before_ENOSPC"] = offset
        assert not staged.exists() and not (root / "pending.bin").exists(), "partial_upload_not_removed"
        policy = good(worker.call("file_worker_info"))
        assert policy["active_transfers"] == 0 and policy["staged_upload_files"] == 0, "cleanup_policy_mismatch"
        assert sha((root / "published.bin").read_bytes()) == sha(sentinel), "published_file_changed"
        report["staging_removed"] = True
        report["published_sentinel_sha256"] = sha(sentinel)
        report["published_sentinel_unchanged"] = True
        assert "error" in worker.call("file_upload_commit", token=token), "failed_token_revived"
        filler.unlink()
        recovered = bytes([0x63]) * 65536
        assert upload(worker, "recovered.bin", recovered)["published"] is True
        assert (root / "recovered.bin").read_bytes() == recovered, "recovery_upload_changed"
        assert (root / "published.bin").read_bytes() == sentinel, "old_publication_lost_after_recovery"
        report["same_worker_recovers_after_space_released"] = True
        worker.close()
        worker = None
        report["cleanup"]["worker_exit_zero"] = True
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)[:800]
    finally:
        if worker:
            try:
                worker.close()
                report["cleanup"]["worker_exit_zero"] = True
            except Exception as error:
                report["passed"] = False
                report["cleanup"]["worker_error"] = str(error)[:200]
        try:
            attached = attached_image(image)
            if attached and device is None:
                candidates = [e["dev-entry"] for e in attached[0]["system-entities"] if re.fullmatch(r"/dev/disk\d+", e.get("dev-entry", ""))]
                if candidates:
                    device = candidates[0]
            assert len(attached) <= 1, "ambiguous_owned_image"
        except Exception as error:
            report["passed"] = False
            report["cleanup"]["inspection_error"] = str(error)[:200]
        if device:
            try:
                command(["/usr/bin/hdiutil", "detach", device])
                report["cleanup"]["image_detached"] = True
            except Exception as error:
                report["passed"] = False
                report["cleanup"]["detach_error"] = str(error)[:400]
        if not report["cleanup"].get("inspection_error") and not os.path.ismount(mount) and (not device or report["cleanup"].get("image_detached")):
            try:
                shutil.rmtree(scratch)
                report["cleanup"]["private_scratch_removed"] = True
            except Exception as error:
                report["passed"] = False
                report["cleanup"]["scratch_error"] = str(error)[:200]
        else:
            report["cleanup"]["retained_scratch"] = str(scratch)
        with os.fdopen(fd, "w") as output:
            json.dump(report, output, indent=2)
            output.write("\n")
    print(json.dumps({"passed": report["passed"], "report": str(args.output.resolve())}))
    return 0 if report["passed"] else 1

if __name__ == "__main__":
    raise SystemExit(main())
