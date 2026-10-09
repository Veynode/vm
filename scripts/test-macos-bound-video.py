#!/usr/bin/env python3
"""Real bound Guest capture -> required hardware H264/HEVC -> FFmpeg decode.

No browser, input, permission changes or performance claim. Compressed desktop
data exists only in private temporary files, deleted at test end.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import time


def run(encoder, source, identity, codec, mismatch, directory, result):
    identity = dict(identity)
    if mismatch:
        identity["security_session_id"] ^= 1
    expected = directory / (codec + ("-mismatch" if mismatch else "") + ".json")
    with os.fdopen(os.open(expected, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "w") as output:
        json.dump(identity, output)
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as diagnostics:
        process = subprocess.Popen([str(encoder), str(source), "--codec", codec,
                                    "--expected-console-identity", str(expected)],
                                   stdin=subprocess.PIPE, stdout=output, stderr=diagnostics)
        start = time.monotonic()
        stop_sent = False
        try:
            while process.poll() is None:
                elapsed = time.monotonic() - start
                assert elapsed < 12, "encoder_deadline"
                assert os.fstat(output.fileno()).st_size <= 32 * 1024 * 1024, "encoder_output_limit"
                assert os.fstat(diagnostics.fileno()).st_size <= 65536, "encoder_diagnostic_limit"
                if not mismatch and elapsed >= 3 and not stop_sent:
                    process.stdin.write(b"stop\n")
                    process.stdin.flush()
                    stop_sent = True
                time.sleep(0.02)
            code = process.wait(timeout=1)
        finally:
            if process.poll() is None:
                result["forced_process_cleanup"] = True
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=2)
            process.stdin.close()
        output.seek(0)
        data = output.read(32 * 1024 * 1024 + 1)
        diagnostics.seek(0)
        logs = diagnostics.read(65537)
    assert len(data) <= 32 * 1024 * 1024 and len(logs) <= 65536, "encoder_final_output_limit"
    events = [json.loads(line) for line in logs.decode().splitlines() if line.startswith("{")]
    result.update(exit_code=code, events=events, encoded_bytes=len(data))
    assert events and events[-1]["guest_cleanup_confirmed"] is True, "guest_cleanup_unconfirmed"
    result["cleanup_confirmed"] = True
    summary = events[-1]
    assert summary["codec"] == codec and summary["console_identity_binding_requested"] is True, "host_binding_not_requested"
    if mismatch:
        assert code == 78 and not data, "mismatch_encoded_output_or_exit"
        assert events[-1]["guest_error"]["code"] == "console_identity_mismatch", "unexpected_mismatch_error"
        assert events[-1]["cleanup_basis"] == "no_stream_created", "mismatch_created_stream"
        assert summary["event"] == "error" and summary["expected_console_identity_matched"] is False, "mismatch_host_claim"
        return result
    assert code == 0 and stop_sent, "encoder_did_not_stop_normally"
    assert summary["event"] == "stopped" and summary["expected_console_identity_matched"] is True, "host_binding_not_verified"
    assert events[-1]["hardware_verified"] is True, "hardware_not_verified"
    started = [e["geometry"] for e in events if e["event"] == "guest-started"]
    assert len(started) == 1 and started[0]["console_identity"] == identity, "host_started_identity_mismatch"
    assert started[0]["console_binding_mode"] == "expected_identity_matched", "host_binding_missing"
    raw = directory / (codec + ".annexb")
    frames = 0
    offset = 0
    last_pts = -1
    with os.fdopen(os.open(raw, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "wb") as bitstream:
        while offset < len(data):
            assert offset + 13 <= len(data), "truncated_au_header"
            length, pts, key = struct.unpack(">IQB", data[offset:offset + 13])
            offset += 13
            assert 0 < length <= 4 * 1024 * 1024 and offset + length <= len(data), "invalid_au_length"
            assert key in (0, 1) and pts > last_pts, "invalid_au_timestamp"
            assert frames or key == 1, "first_au_not_keyframe"
            assert frames or pts == 0, "first_au_timestamp_not_zero"
            bitstream.write(data[offset:offset + length])
            offset += length
            last_pts = pts
            frames += 1
    assert frames > 0 and frames == events[-1]["encoded"], "encoded_count_mismatch"
    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    assert ffmpeg and ffprobe, "ffmpeg_required"
    decode = subprocess.run([ffmpeg, "-v", "error", "-xerror", "-f", codec, "-i", str(raw), "-f", "null", "-"], capture_output=True, timeout=15)
    assert decode.returncode == 0 and not decode.stderr, "independent_decode_failed"
    probe = subprocess.run([ffprobe, "-v", "error", "-f", codec, "-count_frames", "-show_entries", "stream=codec_name,width,height,nb_read_frames", "-of", "json", str(raw)], capture_output=True, timeout=15)
    assert probe.returncode == 0 and not probe.stderr, "independent_probe_failed"
    streams = json.loads(probe.stdout)["streams"]
    assert len(streams) == 1 and int(streams[0]["nb_read_frames"]) == frames, "decoded_frame_count_mismatch"
    expected_size = (1920, 1080) if codec == "hevc" else (1280, 720)
    assert (summary["width"], summary["height"]) == expected_size, "summary_geometry_mismatch"
    assert (streams[0]["width"], streams[0]["height"]) == expected_size, "decoded_geometry_mismatch"
    assert streams[0]["codec_name"] == codec, "decoded_codec_mismatch"
    result.update(encoded_frames=frames, independent_decoder=streams[0], compressed_sha256=hashlib.sha256(data).hexdigest())
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("encoder", "socket", "identity-report", "output", "binary", "state-dir"):
        parser.add_argument("--" + option, type=Path, required=True)
    args = parser.parse_args()
    fd = os.open(args.output, os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_WRONLY, 0o600)
    report = {"passed": False, "scope": "actual Guest capture and host hardware encoding correctness", "performance_validated": False,
              "browser_validated": False, "pixel_content_validated": False, "input_performed": False, "results": []}
    try:
        evidence = json.loads(args.identity_report.read_text())
        assert evidence["passed"] is True, "identity_report_failed"
        identity = evidence["console_identity"]
        binding = evidence["binding"]
        assert args.socket.absolute() == Path(binding["socket_path"]), "bridge_socket_path_mismatch"
        def verify_bridge():
            response = subprocess.run([str(args.binary.resolve()), "call", "--state-dir", str(args.state_dir.resolve()),
                                       "guest_stream_bridge_binding", json.dumps({"id": binding["vm_id"], **{k: binding[k] for k in ("worker_instance", "bridge_instance")}})], capture_output=True, timeout=15)
            assert response.returncode == 0 and json.loads(response.stdout)["binding"] == binding, "bridge_binding_changed"
        report["binding"] = binding
        report["encoder_sha256"] = hashlib.sha256(args.encoder.read_bytes()).hexdigest()
        with tempfile.TemporaryDirectory(prefix="veynode-bound-video-") as folder:
            for codec, mismatch in [("h264", True), ("h264", False), ("hevc", False)]:
                verify_bridge()
                result = {"codec": codec, "expected_mismatch": mismatch, "cleanup_confirmed": False}
                report["results"].append(result)
                run(args.encoder.resolve(), args.socket.resolve(), identity, codec, mismatch, Path(folder), result)
                verify_bridge()
        report["passed"] = True
    except Exception as error:
        import re
        message = str(error)
        report["error"] = message if isinstance(error, AssertionError) and re.fullmatch(r"[a-z][a-z0-9_]{0,79}", message) else type(error).__name__
    finally:
        with os.fdopen(fd, "w") as output:
            json.dump(report, output, indent=2)
            output.write("\n")
    print(json.dumps({"passed": report["passed"], "output": str(args.output.resolve())}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
