#!/usr/bin/env python3
"""Adversarial protocol fixture ONLY: no SPICE, VM, GPU or real codec work."""
import json
import os
import struct
import sys
import time

if sys.argv[1:] == ["--probe-codecs"]:
    print(json.dumps({"h264": "verified-hardware", "h265": "unavailable",
                      "source": "TEST-ONLY PROTOCOL FIXTURE; NOT HARDWARE EVIDENCE"}))
    raise SystemExit(0)

mode = os.environ.get("CONSOLE_TEST_MODE", "normal")
log_path = os.environ["CONSOLE_TEST_LOG"]
active = None
previous = 0
starts = 0

def log(value):
    with open(log_path, "a") as file:
        file.write(json.dumps(value) + "\n")

def record(kind, epoch, body):
    if kind == 1:
        body = json.dumps(body).encode()
    return struct.pack(">IBQ", len(body), kind, epoch) + body

def au(epoch):
    return record(2, epoch, struct.pack(">QB", 1000, 1) + b"\x00\x00\x00\x01\x65test")

log({"event": "spawn", "pid": os.getpid()})
os.write(1, record(1, 0, {"event": "ready", "protocol": 1, "capture_connected": True}))
for line in sys.stdin:
    request = json.loads(line)
    log(request)
    ident, method = request["id"], request["method"]
    epoch = request.get("epoch", 0)
    if method == "start":
        starts += 1
        if active is not None:
            os.write(1, record(1, epoch, {"id": ident, "error": "encoding_busy"}))
            continue
        active = epoch
        ack = record(1, epoch, {"id": ident, "result": {"state": "encoding", "epoch": epoch}})
        if mode == "truncated-au":
            os.write(1, ack + au(epoch)[:6])
            break
        elif mode == "session-error" and starts == 1:
            active = None
            os.write(1, ack + record(1, epoch, {"event": "session-error", "epoch": epoch, "error": "injected encoder failure"}))
        elif mode == "mismatched-session-error":
            os.write(1, ack + record(1, epoch, {"event": "session-error", "epoch": epoch + 1, "error": "wrong session; encoder remains active"}))
        else:
            # Old epoch plus new AU before native ACK, then another AU after it:
            # external start ACK must nevertheless precede every forwarded AU.
            stale = (au(previous) + record(1, previous, {"event": "session-error", "epoch": previous,
                      "error": "old session ended"})) if previous else b""
            os.write(1, stale + au(epoch) + ack + au(epoch))
        previous = epoch
    elif method == "stop":
        if mode == "malformed-stop":
            os.write(1, record(1, epoch, {"id": ident}))
            continue
        if mode == "wrong-stop-state":
            os.write(1, record(1, epoch, {"id": ident, "result": {"state": "encoding"}}))
            continue
        if mode == "stop-timeout":
            # Retain the process and encoder state, but never acknowledge stop.
            continue
        if mode == "delayed-stop":
            # Ensure the shim's EOF-generated second stop is queued before ACK.
            time.sleep(0.2)
        if mode == "stop-error":
            # Keep the backend active. This is an actual failure-injection test
            # of acknowledgement semantics, not a fake successful cleanup.
            os.write(1, record(1, epoch, {"id": ident, "error": "injected stop failure; still active"}))
        elif active != epoch:
            os.write(1, record(1, epoch, {"id": ident, "error": "stale_or_inactive_epoch"}))
        else:
            active = None
            os.write(1, record(1, epoch, {"id": ident, "result": {"state": "stopped"}}))
    elif method == "keyframe":
        if active != epoch:
            os.write(1, record(1, epoch, {"id": ident, "error": "stale_or_inactive_epoch"}))
        else:
            os.write(1, record(1, epoch, {"id": ident, "result": {"requested": True}}))
    elif method == "shutdown":
        active = None
        os.write(1, record(1, 0, {"id": ident, "result": {"state": "stopped"}}))
        break
    else:
        os.write(1, record(1, epoch, {"id": ident, "error": "unsupported fixture command"}))
log({"event": "exit", "active": active})
