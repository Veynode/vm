#!/usr/bin/env python3
"""Scripted peers of the QEMU worker for tests/screenshot_path.rs. One file, four roles chosen by argv:
QEMU (QMP only), the console helper behind a real vm-console, a whole console broker, and their capability probes.
The picture is random bytes behind a PNG signature: protocol test data, never a Guest screen. A request of the
scaled-JPEG form is answered as `scaled` in `peers.json` says (default: a JPEG header of the size the helper's
divisor rule gives, padded to `jpeg_bytes`); every request line the broker reads is appended to `requests.log`.
`peers.json` beside this file is re-read for every screenshot; `peers.log` gets one line of CLOCK_MONOTONIC stamps."""
import base64, json, os, random, socket, struct, sys, threading, time

HERE = os.path.dirname(os.path.realpath(__file__))
CAPS = {"native_window": False, "native_window_protocol": 1, "display_only": True, "input_enabled": False}
BROKER_CAPS = dict(CAPS, broker_window_protocol=1, broker_native_input_protocol=1,
                   encoder_fps_request_supported=False, broker_screenshot_scaled_protocol=1,
                   h264="unavailable", h265="unavailable")


def now():
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)


def config():
    with open(os.path.join(HERE, "peers.json")) as f:
        return json.load(f)


def log(role, *stamps):
    with open(os.path.join(HERE, "peers.log"), "a") as f:
        f.write(role + " " + " ".join(str(s) for s in stamps) + "\n")


_pictures = {}


def picture(cfg):
    """The screenshot result object as JSON bytes, built once per size; also saved for the byte-identity test."""
    key = (cfg["png_bytes"], cfg.get("declared", cfg["png_bytes"]))
    if key not in _pictures:
        png = b"\x89PNG\r\n\x1a\n" + random.Random(cfg["png_bytes"]).randbytes(cfg["png_bytes"] - 8)
        result = {"byte_length": key[1], "captured_at_us": 1, "captured_monotonic_us": 2,
                  "coordinate_space": "guest-physical-pixels", "data_base64": base64.b64encode(png).decode(),
                  "height": cfg["height"], "mime_type": "image/png", "source_generation": 3, "source_sequence": 4,
                  "width": cfg["width"]}
        _pictures[key] = json.dumps(result, separators=(",", ":")).encode()
        with open(os.path.join(HERE, "sent-%d.json" % cfg["png_bytes"]), "wb") as f:
            f.write(_pictures[key])
    return _pictures[key]


DIVISORS = (1, 2, 3, 4, 6, 8, 12, 16)


def scaled_picture(cfg, request):
    """The console helper's reply to a request of the scaled form, as JSON bytes. `scaled` selects a fault:
    `wrong_dimensions` (the JPEG header disagrees with the declared frame), `wrong_divisor`, `wrong_quality`,
    `ignored_bound` (a full-size frame whatever was asked) and `declared` (byte_length overridden)."""
    fault = cfg.get("scaled", "")
    width, height = cfg["width"], cfg["height"]
    divisor = next(d for d in DIVISORS if fault == "ignored_bound" or (
        -(-width // d) <= request["max_width"] and -(-height // d) <= request["max_height"]
        and -(-width // d) % 2 == 0 and -(-height // d) % 2 == 0))
    frame = (-(-width // divisor), -(-height // divisor))
    coded = (frame[0], frame[1] + 2) if fault == "wrong_dimensions" else frame
    jpeg = (b"\xff\xd8\xff\xc0\x00\x11\x08" + struct.pack(">HH", coded[1], coded[0])
            + b"\x03\x01\x22\x00\x02\x11\x01\x03\x11\x01")
    left = cfg.get("jpeg_bytes", 4096) - len(jpeg) - 2
    filler = random.Random(left)
    while left > 0:
        take = min(left - 4, 60000)
        if 0 < left - take - 4 < 4:
            take -= 4
        # A comment segment of bytes below 0x80: never a marker.
        jpeg += b"\xff\xfe" + struct.pack(">H", take + 2) + bytes(filler.randrange(32, 127) for _ in range(take))
        left -= take + 4
    jpeg += b"\xff\xd9"
    result = {"byte_length": cfg.get("declared", len(jpeg)), "captured_at_us": 1, "captured_monotonic_us": 2,
              "coordinate_space": "guest-physical-pixels", "data_base64": base64.b64encode(jpeg).decode(),
              "frame_height": frame[1], "frame_quality": request["quality"] + (1 if fault == "wrong_quality" else 0),
              "frame_width": frame[0], "height": height, "mime_type": "image/jpeg",
              "scale_divisor": 5 if fault == "wrong_divisor" else divisor, "source_generation": 3,
              "source_sequence": 4, "width": width}
    body = json.dumps(result, separators=(",", ":")).encode()
    with open(os.path.join(HERE, "sent-scaled.json"), "wb") as f:
        f.write(body)
    return body


def record(body):
    return struct.pack(">IBQ", len(body), 1, 0) + body


def write_all(fd, data):
    view = memoryview(data)
    while view:
        view = view[os.write(fd, view):]


def qemu(args):
    def value(name):
        return args[args.index(name) + 1]
    paths = [value("-qmp").split(",")[0][len("unix:"):],
             [p for p in value("-spice").split(",") if p.startswith("addr=")][0][5:],
             [p for a in args if a.startswith("socket,id=veynode-agent") for p in a.split(",")
              if p.startswith("path=")][0][5:]]
    listeners = []
    for path in paths:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.bind(path)
        s.listen(4)
        listeners.append(s)
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


def helper():
    write_all(1, record(b'{"event":"ready","protocol":1,"capture_connected":true}'))
    busy_until = 0
    for line in sys.stdin.buffer:
        received = now()
        command = json.loads(line)
        if command["method"] == "screenshot" and received < busy_until:
            # The native helper refuses until its 10 ms owner tick has seen the previous reply written.
            busy = {"code": "screenshot_busy", "message": "screenshot_busy"}
            write_all(1, record(json.dumps({"id": command["id"], "error": busy}).encode()))
            log("busy", received)
            continue
        if command["method"] == "shutdown":
            write_all(1, record(json.dumps({"id": command["id"], "result": {"state": "stopped"}}).encode()))
            return
        if command["method"] != "screenshot":
            write_all(1, record(json.dumps({"id": command["id"], "error": {"code": "unsupported"}}).encode()))
            continue
        cfg = config()
        body = b'{"id":' + json.dumps(command["id"]).encode() + b',"result":' + picture(cfg) + b"}"
        wire = record(body)
        time.sleep(max(0.0, cfg["delay_ms"] / 1000 - (now() - received) / 1e9))
        started = now()
        write_all(1, wire)
        busy_until = now() + cfg.get("busy_ms", 0) * 1_000_000
        log("helper", received, started, now(), len(wire))


def broker(args):
    path = args[2]
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(path)
    listener.listen(8)
    # `old_broker`: a console broker from before the scaled form, which does not announce it.
    caps = {k: v for k, v in BROKER_CAPS.items()
            if k != "broker_screenshot_scaled_protocol" or not config().get("old_broker")}
    sys.stdout.write(json.dumps({"event": "ready", "protocol": 1, "console_socket": path, "capture_connected": True,
                                 "guest_frame_available": None, "capabilities": caps}) + "\n")
    sys.stdout.flush()

    def serve(conn):
        line = conn.makefile("rb").readline()
        received = now()
        request = json.loads(line)
        cfg = config()
        with open(os.path.join(HERE, "requests.log"), "ab") as f:
            f.write(line)
        prefix = b'{"request_id":' + json.dumps(request["request_id"]).encode() + b',"result":'
        scaled = cfg.get("scaled", "")
        if request["method"] != "screenshot":
            wire = record(prefix + b"null}")
        elif "format" in request and scaled.startswith("refuse:"):
            # What vm-console answers when the helper refuses the scaled form: its text and the fixed word.
            wire = record(json.dumps({"request_id": request["request_id"],
                                      "error": "native operation failed: scripted",
                                      "screenshot_refusal": scaled[len("refuse:"):]}).encode())
        elif "format" in request and scaled != "png":
            wire = record(prefix + scaled_picture(cfg, request) + b"}")
        elif cfg.get("mode") == "oversize_frame":
            wire = struct.pack(">IBQ", 12 * 1024 * 1024 + 1, 1, 0) + b"x" * 65536
        else:
            wire = record(prefix + picture(cfg) + b"}")
        time.sleep(max(0.0, cfg["delay_ms"] / 1000 - (now() - received) / 1e9))
        started = now()
        try:
            conn.sendall(wire)
        except OSError:
            pass
        log("broker", received, started, now(), len(wire))
        conn.close()

    while True:
        conn, _ = listener.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


def main():
    args = sys.argv[1:]
    os.umask(0o077)
    if "-qmp" in args:
        qemu(args)
    elif args[:1] == ["--probe-capabilities"]:
        print(json.dumps(BROKER_CAPS if len(args) == 2 else CAPS))
    elif args[:1] == ["--probe-codecs"]:
        print(json.dumps({"h264": "unavailable", "h265": "unavailable"}))
    elif len(args) >= 2 and args[1] == "--persistent":
        helper()
    else:
        broker(args)


if __name__ == "__main__":
    main()
