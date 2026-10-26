#!/usr/bin/env python3
"""QMP/console-only peers copied from screenshot_path_peers.py; never boots a Guest."""
import base64
import json
import os
import socket
import struct
import sys

CAPS = {"native_window": False, "native_window_protocol": 1, "display_only": True,
        "input_enabled": False, "broker_window_protocol": 1,
        "broker_native_input_protocol": 1, "encoder_fps_request_supported": False,
        "h264": "unavailable", "h265": "unavailable"}
# Header-shaped protocol fixture, as in screenshot_path.rs, not a decodable PNG.
PNG = b"\x89PNG\r\n\x1a\n" + b"VNMARK-SCREEN-01!" * 128


def qemu(args):
    def value(name):
        return args[args.index(name) + 1]
    paths = [value("-qmp").split(",")[0][len("unix:"):],
             [p[5:] for p in value("-spice").split(",") if p.startswith("addr=")][0],
             [p[5:] for a in args if a.startswith("socket,id=veynode-agent")
              for p in a.split(",") if p.startswith("path=")][0]]
    listeners = []
    for path in paths:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(path)
        listener.listen(4)
        listeners.append(listener)
    running = "-S" not in args
    conn, _ = listeners[0].accept()
    conn.sendall(b'{"QMP":{"version":{},"capabilities":[]}}\n')
    for line in conn.makefile("rb"):
        request = json.loads(line)
        reply = {}
        if request["execute"] == "cont":
            running = True
        if request["execute"] == "query-status":
            reply = {"status": "running" if running else "prelaunch", "running": running}
        conn.sendall(json.dumps({"return": reply, "id": request["id"]}).encode() + b"\n")
        if request["execute"] == "quit":
            break


def broker(args):
    path = args[2]
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(path)
    listener.listen(4)
    print(json.dumps({"event": "ready", "protocol": 1, "console_socket": path,
                      "capture_connected": True, "guest_frame_available": None,
                      "capabilities": CAPS}), flush=True)
    while True:
        conn, _ = listener.accept()
        with conn:
            conn.settimeout(2)
            request = json.loads(conn.makefile("rb").readline())
            result = None
            if request["method"] == "screenshot":
                result = {"byte_length": len(PNG), "captured_at_us": 1,
                          "captured_monotonic_us": 2, "coordinate_space": "guest-physical-pixels",
                          "data_base64": base64.b64encode(PNG).decode(), "height": 800,
                          "mime_type": "image/png", "source_generation": 3,
                          "source_sequence": 4, "width": 1280}
            body = json.dumps({"request_id": request["request_id"], "result": result}).encode()
            conn.sendall(struct.pack(">IBQ", len(body), 1, 0) + body)


if __name__ == "__main__":
    os.umask(0o077)
    args = sys.argv[1:]
    if "-qmp" in args:
        qemu(args)
    elif args[:1] == ["--probe-capabilities"]:
        print(json.dumps(CAPS))
    elif args[:1] == ["--probe-codecs"]:
        print(json.dumps({"h264": "unavailable", "h265": "unavailable"}))
    else:
        broker(args)
