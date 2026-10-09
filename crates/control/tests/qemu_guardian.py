import json, os, signal, socket, subprocess, sys, time

checks = []
for stopped in (False, True):
    parent, child = socket.socketpair()
    guard = subprocess.Popen([sys.argv[1], '--supervise-qemu', str(child.fileno()), '--', '/bin/sleep', '30'], pass_fds=(child.fileno(),), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    child.close()
    writer = None
    try:
        parent.settimeout(3)
        data = b''
        while len(data) < 4:
            chunk = parent.recv(4-len(data))
            assert chunk
            data += chunk
        owned_pid = int.from_bytes(data, 'big')
        challenge = b''
        while len(challenge)<8:
            challenge += parent.recv(8-len(challenge))
        parent.sendall(challenge)
        if stopped:
            writer = os.fork()
            if writer == 0:
                os.kill(os.getpid(), signal.SIGSTOP)
                os._exit(0)
            _, status = os.waitpid(writer, os.WUNTRACED)
            assert os.WIFSTOPPED(status)
        parent.close()  # A stopped worker still owns its copy; only host-time expiry works.
        started = time.monotonic()
        out, err = guard.communicate(timeout=8)
        assert guard.returncode != 0
        elapsed = time.monotonic()-started
        assert elapsed < 7
        try:
            os.kill(owned_pid, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError('guardian did not reap owned child')
        if stopped:
            assert elapsed >= 4
        checks.append({'stopped_owner':stopped, 'guardian_exit':guard.returncode, 'child_reaped':True, 'elapsed_seconds':elapsed})
    finally:
        parent.close()
        if writer is not None:
            os.kill(writer, signal.SIGKILL)
            os.waitpid(writer, 0)
        if guard.poll() is None:
            # Closing all owned endpoints must let guardian kill/reap. Never kill only guardian.
            guard.communicate(timeout=8)
print(json.dumps({'passed':True,'scope':'real owned /bin/sleep child, no VM','checks':checks}))
