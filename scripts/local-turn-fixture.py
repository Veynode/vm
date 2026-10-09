#!/usr/bin/env python3
"""Disposable loopback-only coturn REST fixture, never a deployment tool.

Creates only a new private directory. Secrets remain in private files; stdout
contains paths/expiry only. SIGTERM, SIGINT or the deadline reaps its child.
"""
import argparse
import base64
import hashlib
import hmac
import json
import os
import pathlib
import secrets
import signal
import socket
import subprocess
import time


def private(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as file:
        file.write(data)


def mint(directory, name, expired=False):
    if not name.isascii() or not name.replace("-", "").isalnum() or len(name) > 64:
        raise ValueError("simple output name of at most64 characters required")
    info = json.loads((directory / "fixture-private.json").read_text())
    now = int(time.time())
    expires = now + 600
    username = f'{now - 60 if expired else expires}:{secrets.token_hex(12)}'
    credential = base64.b64encode(
        hmac.new(bytes.fromhex(info["secret"]), username.encode(), hashlib.sha1).digest()
    ).decode()
    config = {
        "version": 1,
        "transport_policy": "relay",
        "issued_at": now,
        "expires_at": expires,
        "ice_servers": [{
            "urls": [f'turn:127.0.0.1:{info["port"]}?transport=udp'],
            "username": username,
            "credential": credential,
        }],
    }
    path = directory / (name + ".json")
    private(path, json.dumps(config))
    print(json.dumps({
        "ice_config_path": str(path), "expires_at": expires,
        "loopback_only": True, "credential_expired_fixture": expired,
    }), flush=True)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["serve", "mint"])
    parser.add_argument("directory", type=pathlib.Path)
    parser.add_argument("--name", default="ice")
    parser.add_argument("--expired-credential", action="store_true")
    parser.add_argument("--seconds", type=int, default=900)
    parser.add_argument("--max-bytes-per-second", type=int, default=2_500_000,
                        help="Coturn per-allocation, per-direction byte cap, 1024..2500000; "
                             "excess packets are dropped/suppressed, not WAN emulation")
    args = parser.parse_args()
    directory = args.directory.absolute()
    if args.action == "mint":
        if args.max_bytes_per_second != 2_500_000:
            raise ValueError("bandwidth is a serve-only fixture setting")
        stat = directory.lstat()
        if (not directory.is_dir() or stat.st_uid != os.geteuid()
                or stat.st_mode & 0o077 or directory.is_symlink()):
            raise ValueError("private owner-held fixture directory required")
        mint(directory, args.name, args.expired_credential)
        return
    if args.expired_credential:
        raise ValueError("expired credential is an explicit mint-only negative fixture")
    if not 10 <= args.seconds <= 1800:
        raise ValueError("fixture lifetime must be10..1800seconds")
    if not 1024 <= args.max_bytes_per_second <= 2_500_000:
        raise ValueError("per-allocation byte cap must be1024..2500000")
    directory.mkdir(mode=0o700, parents=False, exist_ok=False)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    # coturn consumes this random secret as ASCII; mint must use the exact bytes.
    secret_text = secrets.token_hex(32)
    private(directory / "fixture-private.json", json.dumps({
        "secret": secret_text.encode().hex(), "port": port,
    }))
    config = [
        f"listening-port={port}", "listening-ip=127.0.0.1", "relay-ip=127.0.0.1",
        "no-tcp", "no-tls", "allow-loopback-peers", "no-tcp-relay",
        "denied-peer-ip=0.0.0.0-127.0.0.0", "denied-peer-ip=127.0.0.2-255.255.255.255",
        "max-allocate-lifetime=600", "verbose", "no-multicast-peers", "no-rfc5780",
        "relay-threads=0", "use-auth-secret", f"static-auth-secret={secret_text}",
        "realm=veynode-local-fixture", f"userdb={directory}/turndb",
        "fingerprint", "user-quota=4", "total-quota=8",
        f"max-bps={args.max_bytes_per_second}", "bps-capacity=10000000", "min-port=51000", "max-port=51100",
        f"pidfile={directory}/turn.pid", "no-stdout-log",
        f"log-file={directory}/turn.log", "simple-log",
    ]
    private(directory / "turn.conf", "\n".join(config) + "\n")
    with open(directory / "process.log", "xb") as log:
        process = subprocess.Popen(
            ["/opt/homebrew/opt/coturn/bin/turnserver", "-c", str(directory / "turn.conf")],
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        )
        stopped = False

        def stop(*_):
            nonlocal stopped
            stopped = True

        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGINT, stop)
        try:
            time.sleep(0.3)
            if process.poll() is not None:
                raise RuntimeError("local coturn failed; inspect private fixture log")
            private(directory / "ready.json", json.dumps({
                "pid": process.pid, "supervisor_pid": os.getpid(), "port": port,
                "loopback_only": True,
                "bandwidth": {
                    "per_allocation_per_direction_bytes_per_second": args.max_bytes_per_second,
                    "server_capacity_bytes_per_second": 10_000_000,
                    "mechanism": "coturn-max-bps-drop-or-suppress",
                    "wan_delay_jitter_loss_emulated": False,
                    "delivered_bandwidth_measured": False,
                },
            }))
            mint(directory, args.name)
            deadline = time.monotonic() + args.seconds
            while not stopped and time.monotonic() < deadline and process.poll() is None:
                time.sleep(0.1)
            if process.poll() is not None and not stopped:
                raise RuntimeError("local coturn exited unexpectedly")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(2)
            private(directory / "stopped.json", json.dumps({
                "reaped": True, "returncode": process.returncode,
            }))


if __name__ == "__main__":
    main()
