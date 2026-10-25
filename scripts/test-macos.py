#!/usr/bin/env python3
"""Real-host acceptance for macOS containers (Tart images on Apple Virtualization).

A Tart macOS image is ~25 GB to download and ~40 GB on disk, so this test
uses an existing VEYNODEVM_HOME (with a running daemon) instead of a fresh
one, and pulls IMAGE there only if it is missing. It checks: run -d with a
background COMMAND and logs, --disk-size/--dns, exec (sw_vers, Metal GPU),
stdin only with -i, the GPU at game load (scripts/macos-gpu-bench.swift
offscreen against the same run on the host, and on screen in the Guest GUI
session with vsync), no Setup Assistant over the desktop, the container
hostname, a GUI app started with `open`, -v (virtiofs mounted read/write),
-p, cp in both
directions, MCP container_exec/container_logs, vm_screenshot and
vm_input_click through Guest Screen Sharing (delivery checked with the Guest
HID idle timer), stop/start (the COMMAND runs again), rm, and a foreground
run --rm that returns the COMMAND's status. Writes a JSON summary to stdout.

  VEYNODEVM_HOME=... python3 scripts/test-macos.py [--image REF]
"""
import argparse, base64, json, os, pathlib, re, subprocess, sys, tempfile, time, urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CLI = ROOT / 'target/debug/veynodevm'
NAME = 'mac-accept'
COMMAND = ['sh', '-c', 'echo veynodevm-ready; sleep 86400']
SERVER = "nohup sh -c 'while true; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nveynodevm-macos\\n\" | nc -l 8080 >/dev/null; done' >/dev/null 2>&1 &"


def cli(*args, check=True, timeout=600):
    result = subprocess.run([str(CLI), *args], capture_output=True, text=True, timeout=timeout)
    if check and result.returncode != 0:
        raise SystemExit(f'veynodevm {" ".join(args)} failed ({result.returncode}): {result.stderr.strip()}')
    return result


def http(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            return urllib.request.urlopen(f'http://127.0.0.1:{port}/', timeout=3).read().decode()
        except OSError:
            time.sleep(1)
    raise SystemExit(f'no HTTP answer on 127.0.0.1:{port}')


def record():
    return next(c for c in json.loads(cli('--json', 'ps', '-a').stdout) if c['name'] == NAME)


def idle_seconds():
    out = cli('exec', NAME, '/usr/sbin/ioreg', '-c', 'IOHIDSystem', '-d', '4').stdout
    return int(re.search(r'"HIDIdleTime" = (\d+)', out).group(1)) / 1e9


def gpu(share):
    """Game-like Metal load: offscreen throughput against the host, then on
    screen in the Guest's GUI session (no SSH session owns the display)."""
    bench = pathlib.Path(share, 'gpu-bench')
    subprocess.run(['xcrun', 'swiftc', '-O', '-suppress-warnings', '-target', 'arm64-apple-macosx14.0',
                    str(ROOT / 'scripts/macos-gpu-bench.swift'), '-o', str(bench)], check=True)
    host = json.loads(subprocess.run([str(bench), '--seconds', '5'], capture_output=True, text=True, check=True).stdout)
    cli('cp', str(bench), f'{NAME}:/Users/Shared/gpu-bench')
    assert cli('exec', NAME, 'sh', '-c', 'pgrep -f MiniBuddy || true').stdout == '', 'Setup Assistant is running'
    assert cli('exec', NAME, 'scutil', '--get', 'LocalHostName').stdout.strip() == NAME
    # Let post-boot indexing settle; the first seconds after boot are noisy.
    time.sleep(20)
    guest = json.loads(cli('exec', NAME, '/Users/Shared/gpu-bench', '--seconds', '5').stdout)
    assert guest['ok'] and guest['fps'] >= 60, guest
    cli('exec', NAME, 'sh', '-c', 'sudo -n launchctl asuser $(id -u) /Users/Shared/gpu-bench --mode window '
        '--seconds 6 --output /Users/Shared/window.json >/dev/null')
    window = json.loads(cli('exec', NAME, 'cat', '/Users/Shared/window.json').stdout)
    assert window['ok'] and window['fps'] >= 50 and 15 <= window['pacing_ms_p50'] <= 18.5, window
    cli('exec', NAME, 'open', '-a', 'Chess')
    time.sleep(5)
    assert cli('exec', NAME, 'pgrep', '-x', 'Chess').returncode == 0
    cli('exec', NAME, 'pkill', '-x', 'Chess', check=False)
    return {'gpu': {
        'load': f"{guest['triangles_per_frame']} triangles, {guest['draws']} draws, 4x MSAA, 1920x1080",
        'host_fps': host['fps'], 'guest_offscreen_fps': guest['fps'],
        'guest_vs_host': round(guest['fps'] / host['fps'], 2),
        'guest_window_vsync_fps': window['fps'], 'pacing_ms_p50': window['pacing_ms_p50'],
        'pacing_ms_p99': window['pacing_ms_p99'], 'guest_display_hz': window['screen_max_fps'],
    }, 'gui_app': 'Chess via open -a', 'setup_assistant': 'not running'}


def mcp(vm_id):
    process = subprocess.Popen([str(CLI), 'mcp', '--execute', '--input'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
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

    def tool(name, **arguments):
        reply = rpc('tools/call', {'name': name, 'arguments': arguments})
        if 'error' in reply:
            raise SystemExit(f'{name}: {reply["error"]}')
        result = reply['result']
        if result.get('isError'):
            raise SystemExit(f'{name}: {result["content"][0]["text"]}')
        return result['content']

    rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'test-macos', 'version': '1'}})
    rpc('notifications/initialized', notify=True)
    summary = {}
    out = json.loads(tool('container_exec', container=NAME, command=['sw_vers', '-productVersion'])[0]['text'])
    assert out['exit_code'] == 0, out
    summary['mcp_exec'] = out['stdout'].strip()
    logs = tool('container_logs', container=NAME)[0]['text']
    assert 'veynodevm-ready' in logs, logs
    info = json.loads(tool('vm_guest_info', id=vm_id)[0]['text'])
    summary['guest_transport'] = info.get('result', info).get('transport')
    content = tool('vm_screenshot', id=vm_id)
    frame = json.loads(content[1]['text'])
    image = base64.b64decode(content[0]['data'])
    assert content[0]['mimeType'] == 'image/jpeg' and image[:3] == b'\xff\xd8\xff', content[0]['mimeType']
    shot = pathlib.Path(tempfile.gettempdir()) / 'veynodevm-macos-screenshot.jpg'
    shot.write_bytes(image)
    summary['screenshot'] = {'width': frame['width'], 'height': frame['height'], 'bytes': len(image), 'path': str(shot)}
    time.sleep(3)
    before = idle_seconds()
    tool('vm_acquire_control', id=vm_id)
    content = tool('vm_screenshot', id=vm_id)
    frame = json.loads(content[1]['text'])
    tool('vm_input_click', id=vm_id, x=frame['width'] - 10, y=frame['height'] // 2, frame_id=frame['frame_id'],
         width=frame['width'], height=frame['height'], source_generation=frame['source_generation'])
    after = idle_seconds()
    tool('vm_release_control', id=vm_id)
    assert after < before and after < 5, (before, after)
    summary['input_click'] = {'hid_idle_before_s': round(before, 1), 'hid_idle_after_s': round(after, 1)}
    process.stdin.close()
    process.wait(timeout=10)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--image', default='ghcr.io/cirruslabs/macos-tahoe-vanilla:latest')
    args = parser.parse_args()
    if not os.environ.get('VEYNODEVM_HOME'):
        raise SystemExit('set VEYNODEVM_HOME to a home with a running daemon (the image needs ~40 GB)')
    summary = {'image': args.image}
    cli('rm', '-f', NAME, check=False)
    if cli('inspect', args.image, check=False).returncode != 0:
        cli('pull', args.image, timeout=4 * 3600)
    with tempfile.TemporaryDirectory(prefix='vvm-macos-') as share:
        pathlib.Path(share, 'from-host.txt').write_text('host-file\n')
        started = time.time()
        cli('run', '-d', '--name', NAME, '-v', f'{share}:/Users/Shared/host', '-p', '18080:8080',
            '--disk-size', '64', '--dns', '9.9.9.9', args.image, *COMMAND)
        summary['run_d_with_command_s'] = round(time.time() - started, 1)
        summary['sw_vers'] = cli('exec', NAME, 'sw_vers', '-productVersion').stdout.strip()
        assert cli('logs', NAME).stdout == 'veynodevm-ready\n'
        summary['logs'] = 'background COMMAND output'
        root_gib = int(cli('exec', NAME, 'sh', '-c', "df -g / | awk 'NR==2{print $2}'").stdout)
        assert root_gib >= 55, root_gib
        dns = cli('exec', NAME, 'sh', '-c', 'scutil --dns | grep -m1 "nameserver\\[0\\]"').stdout
        assert '9.9.9.9' in dns, dns
        summary['disk_size_gib'] = root_gib
        summary['dns'] = '9.9.9.9'
        stdin = subprocess.run([str(CLI), 'exec', NAME, 'cat'], input='piped', capture_output=True, text=True, timeout=60)
        stdin_i = subprocess.run([str(CLI), 'exec', '-i', NAME, 'cat'], input='piped', capture_output=True, text=True, timeout=60)
        assert stdin.stdout == '' and stdin_i.stdout == 'piped', (stdin.stdout, stdin_i.stdout)
        summary['stdin'] = 'only with -i'
        probe = pathlib.Path(share, 'metal-check')
        subprocess.run(['xcrun', 'swiftc', '-O', '-target', 'arm64-apple-macosx14.0', str(ROOT / 'scripts/macos-metal-check.swift'), '-o', str(probe)], check=True)
        cli('cp', str(probe), f'{NAME}:/Users/Shared/metal-check')
        summary['metal'] = json.loads(cli('exec', NAME, '/Users/Shared/metal-check').stdout)
        assert summary['metal']['ok'] and summary['metal']['mismatches'] == 0, summary['metal']
        summary.update(gpu(share))
        assert cli('exec', NAME, 'cat', '/Users/Shared/host/from-host.txt').stdout == 'host-file\n'
        cli('exec', NAME, 'sh', '-c', 'echo guest-file > /Users/Shared/host/from-guest.txt')
        assert pathlib.Path(share, 'from-guest.txt').read_text() == 'guest-file\n'
        summary['volume'] = 'read/write'
        cli('exec', NAME, 'sh', '-c', SERVER)
        assert 'veynodevm-macos' in http(18080)
        summary['publish'] = '127.0.0.1:18080 -> 8080'
        local = pathlib.Path(share, 'cp.txt')
        local.write_text('copied\n')
        cli('cp', str(local), f'{NAME}:/Users/Shared/cp.txt')
        cli('cp', f'{NAME}:/Users/Shared/cp.txt', str(pathlib.Path(share, 'back.txt')))
        assert pathlib.Path(share, 'back.txt').read_text() == 'copied\n'
        summary['cp'] = 'both directions'
        vm_id = record()['vm_id']
        summary.update(mcp(vm_id))
        cli('stop', NAME, timeout=180)
        assert record()['status'] == 'Exited'
        cli('start', NAME)
        assert cli('exec', NAME, 'cat', '/Users/Shared/cp.txt').stdout == 'copied\n'
        summary['restart_keeps_disk'] = True
        assert cli('logs', NAME).stdout == 'veynodevm-ready\n' * 2
        summary['restart_reruns_command'] = True
        cli('rm', '-f', NAME, timeout=180)
        foreground = cli('run', '--rm', '--name', NAME, args.image, 'sh', '-c', 'sw_vers -productVersion; exit 7', check=False)
        assert foreground.returncode == 7 and foreground.stdout.strip() == summary['sw_vers'], foreground
        assert not any(c['name'] == NAME for c in json.loads(cli('--json', 'ps', '-a').stdout))
        summary['foreground_run_rm'] = 'exit 7, removed'
    json.dump(summary, sys.stdout, indent=2)
    print()


if __name__ == '__main__':
    main()
