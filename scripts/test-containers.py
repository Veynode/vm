#!/usr/bin/env python3
"""Real-host acceptance for OCI micro-VM containers (Apple Virtualization).

Uses an isolated VEYNODEVM_HOME, registers this checkout's runtime, runs a
foreground daemon and checks: pull, run --rm exit status and output, -d with
-p (curl through the host publisher), -v read/write, exec, stop/start with
republishing, rm, image gc, and the MCP container tools. Needs network access
for the first pull. Writes a JSON summary to stdout.
"""
import json, os, pathlib, shutil, signal, subprocess, sys, tempfile, time, urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CLI = ROOT / 'target/debug/veynodevm'
IMAGE = 'alpine:3.20'
SERVER = 'while true; do printf "HTTP/1.0 200 OK\\r\\n\\r\\nveynodevm\\n" | nc -l -p 8080 >/dev/null; done'


def cli(*args, check=True, env=None, timeout=300):
    result = subprocess.run([str(CLI), *args], capture_output=True, text=True, env=env, timeout=timeout)
    if check and result.returncode != 0:
        raise SystemExit(f'veynodevm {" ".join(args)} failed ({result.returncode}): {result.stderr.strip()}')
    return result


def http(port, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            return urllib.request.urlopen(f'http://127.0.0.1:{port}/', timeout=3).read().decode()
        except OSError:
            time.sleep(0.5)
    raise SystemExit(f'no HTTP answer on 127.0.0.1:{port}')


def mcp(env):
    process = subprocess.Popen([str(CLI), 'mcp', '--lifecycle'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
    counter = 0

    def rpc(method, params=None, notify=False):
        nonlocal counter
        message = {'jsonrpc': '2.0', 'method': method}
        if params is not None:
            message['params'] = params
        if not notify:
            counter += 1
            message['id'] = counter
        process.stdin.write(json.dumps(message) + '\n')
        process.stdin.flush()
        return None if notify else json.loads(process.stdout.readline())

    def tool(tool_name, **arguments):
        result = rpc('tools/call', {'name': tool_name, 'arguments': arguments})['result']
        return result['isError'], result['content'][0]['text']

    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'test', 'version': '1'}})
    rpc('notifications/initialized', notify=True)
    error, text = tool('container_run', image=IMAGE, name='mcp-smoke', command=['sh', '-c', 'echo from-mcp; sleep 60'])
    assert not error, text
    for _ in range(60):
        error, text = tool('container_logs', container='mcp-smoke')
        if 'from-mcp' in text:
            break
        time.sleep(0.5)
    assert not error and 'from-mcp' in text, text
    assert tool('container_rm', container='mcp-smoke', confirm='mcp-smoke')[0], 'rm of running container must fail'
    assert not tool('container_stop', container='mcp-smoke')[0]
    assert not tool('container_rm', container='mcp-smoke', confirm='mcp-smoke')[0]
    assert tool('container_run', image='--privileged')[0], 'option injection must be refused'
    process.stdin.close()
    process.wait(timeout=10)
    return True


def main():
    subprocess.run(['cargo', 'build', '--bins'], cwd=ROOT, check=True)
    home = pathlib.Path(tempfile.mkdtemp(prefix='veynodevm-test-', dir=ROOT / '.local'))
    env = {**os.environ, 'VEYNODEVM_HOME': str(home)}
    subprocess.run([sys.executable, str(ROOT / 'scripts/install-dev-runtime.py')], env=env, check=True, stdout=subprocess.DEVNULL)
    daemon = subprocess.Popen([str(CLI), 'daemon', 'run'], env=env, stdout=open(home / 'daemon.log', 'w'), stderr=subprocess.STDOUT)
    summary = {}
    try:
        for _ in range(50):
            if cli('daemon', 'status', check=False, env=env).returncode == 0:
                break
            time.sleep(0.2)
        started = time.time()
        cli('pull', IMAGE, env=env, timeout=900)
        summary['pull_seconds'] = round(time.time() - started, 1)

        started = time.time()
        run = cli('run', '--rm', IMAGE, 'sh', '-c', 'echo hello; cat /etc/alpine-release; exit 7', check=False, env=env)
        summary['run_rm_seconds'] = round(time.time() - started, 1)
        assert run.returncode == 7, (run.returncode, run.stderr)
        assert run.stdout.startswith('hello\n3.20'), run.stdout
        assert json.loads(cli('--json', 'ps', '-a', env=env).stdout) == [], 'run --rm must remove the container'

        volume = home / 'volume'
        volume.mkdir()
        (volume / 'in.txt').write_text('from-host\n')
        cli('run', '-d', '--name', 'web', '-p', '18781:8080', '-v', f'{volume}:/data', IMAGE,
            'sh', '-c', 'echo from-guest > /data/out.txt; ' + SERVER, env=env)
        assert http(18781).strip() == 'veynodevm'
        assert (volume / 'out.txt').read_text() == 'from-guest\n'
        exec_result = cli('exec', 'web', 'sh', '-c', 'cat /data/in.txt; exit 3', check=False, env=env)
        assert exec_result.returncode == 3 and exec_result.stdout == 'from-host\n', exec_result
        inspect = json.loads(cli('--json', 'inspect', 'web', env=env).stdout)
        summary['guest_ip'] = inspect['ip']

        cli('stop', 'web', env=env)
        assert json.loads(cli('--json', 'ps', env=env).stdout) == []
        cli('start', 'web', env=env)
        assert http(18781).strip() == 'veynodevm', 'publishing must follow a restart'
        cli('rm', '-f', 'web', env=env)

        summary['mcp'] = mcp(env)
        cli('rmi', IMAGE, env=env)
        cli('image', 'gc', env=env)
        assert not any((home / 'images/blobs/sha256').iterdir()), 'gc must reclaim blobs'
        summary['result'] = 'pass'
    finally:
        for c in json.loads(cli('--json', 'ps', '-a', check=False, env=env).stdout or '[]'):
            cli('rm', '-f', c['id'], check=False, env=env)
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=20)
        except subprocess.TimeoutExpired:
            daemon.kill()
    shutil.rmtree(home, ignore_errors=True)
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
