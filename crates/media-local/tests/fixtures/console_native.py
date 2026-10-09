#!/usr/bin/env python3
"""Adversarial protocol fixture ONLY: no SPICE, VM, GPU or real codec work."""
import json
import os
import struct
import sys
import time

if sys.argv[1:] == ["--probe-capabilities"]:
    print(json.dumps({"native_window_protocol":1,"native_window":True,
                      "display_only":True,"input_enabled":False}))
    raise SystemExit(0)

if sys.argv[1:] == ["--probe-codecs"]:
    print(json.dumps({"h264": "unavailable" if os.environ.get("CONSOLE_TEST_MODE") == "window-only" else "verified-hardware", "h265": "unavailable",
                      "rate_control_version":1,"rate_min_bps":250000,"rate_max_bps":40000000,"source": "TEST-ONLY PROTOCOL FIXTURE; NOT HARDWARE EVIDENCE"}))
    raise SystemExit(0)

mode = os.environ.get("CONSOLE_TEST_MODE", "normal")
log_path = os.environ["CONSOLE_TEST_LOG"]
active = None
window_open = False
previous = 0
starts = 0
rate_sequence=0
rate_target=8000000

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
    if method in {"show_window", "close_window", "window_status"}:
        assert set(request) == {"id", "method"}
        if method == "show_window": window_open = True
        if method == "close_window": window_open = False
        os.write(1, record(1, 0, {"id":ident,"result":{"native_window_protocol":1,"native_window":True,"display_only":True,"input_enabled":False,"window_open":window_open}}))
    elif method == "start":
        starts += 1
        if active is not None:
            os.write(1, record(1, epoch, {"id": ident, "error": "encoding_busy"}))
            continue
        active = epoch
        rate_sequence=0
        rate_target=8000000
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
    elif method in {"rate_status","set_bitrate"}:
        instance=format(active or 0,'032x')
        error=None
        if method=="set_bitrate":
            if request['encoder_instance']!=instance: error='stale_encoder'
            elif request['sequence']!=rate_sequence+1: error='stale_sequence'
            else: rate_sequence+=1;rate_target=request['target_bps']
        if error: os.write(1,record(1,epoch,{"id":ident,"error":{"code":error}}))
        else:
            result={"encoder_instance":instance,"epoch":epoch,"sequence":rate_sequence,"target_bps":rate_target,"readback_bps":rate_target,"submitted_frames":2,"hardware_verified":True}
            if mode=='stale-rate-receipt' and method=='set_bitrate': result['sequence']-=1
            os.write(1,record(1,epoch,{"id":ident,"result":result}))
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
