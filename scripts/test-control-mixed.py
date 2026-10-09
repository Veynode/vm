#!/usr/bin/env python3
"""Attach-only actual macOS + QEMU Linux acceptance, never manage the Mac lifecycle.
Requires an operator-started mixed-backend daemon and an already running macOS VM.
Creates one unique RAM-only QEMU fixture; kills only that fixture's owned worker.
The operator supplies an independently attributed Apple XPC helper PID. No benchmark.
"""
import argparse
import base64
import concurrent.futures
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import re
import signal
import stat
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
HELPER = '/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine'
READ_ONLY = {'list', 'status', 'capabilities', 'guest_info', 'screenshot'}
OWNED_MUTATIONS = {'create', 'start', 'shutdown', 'force_stop'}


def authorize(method, payload, owned_id):
    """Every daemon call crosses this guard, including failure-path cleanup."""
    if method in READ_ONLY:
        return
    if method not in OWNED_MUTATIONS or payload.get('id') != owned_id:
        raise ValueError('refusing mutation outside this test-owned VM')
    if method == 'create' and (payload.get('backend') != 'qemu-hvf' or payload.get('guest') != 'linux' or payload.get('disk') or payload.get('machine_identity_dir')):
        raise ValueError('only a diskless QEMU Linux fixture may be created')
    if method == 'force_stop' and payload.get('confirm') != owned_id:
        raise ValueError('owned force stop requires exact confirmation')


def process_table():
    output = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,pgid=,stat=,comm='], text=True)
    rows = {}
    for line in output.splitlines():
        parts = line.strip().split(maxsplit=4)
        if len(parts) == 5:
            rows[int(parts[0])] = dict(parent=int(parts[1]), group=int(parts[2]), state=parts[3], executable=parts[4])
    return rows


def decode_image(body, width, height):
    """A successful container/header parse alone is not image decode evidence."""
    assert type(width) is int and type(height) is int and 0 < width <= 8192 and 0 < height <= 8192
    assert width * height <= 16 * 1024 * 1024
    decoded = subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-xerror', '-i', 'pipe:0',
                              '-map', '0:v:0', '-frames:v', '1', '-f', 'framemd5', 'pipe:1'],
                             input=body, capture_output=True, timeout=15)
    assert decoded.returncode == 0, 'independent screenshot decoder failed'
    text = decoded.stdout.decode('ascii', errors='strict')
    match = re.search(r'^#dimensions 0: (\d+)x(\d+)$', text, re.MULTILINE)
    assert match and tuple(map(int, match.groups())) == (width, height), 'decoded screenshot dimensions differ from metadata'
    frames = [line for line in text.splitlines() if line and not line.startswith('#')]
    assert len(frames) == 1 and len(frames[0].split(',')) == 6, 'decoder did not emit exactly one frame'
    return {'decoder': 'FFmpeg framemd5', 'decoded_frames': 1, 'width': width, 'height': height}


def read_boot_log(path, offset):
    with path.open('rb') as source:
        assert os.fstat(source.fileno()).st_size >= offset, 'serial log truncated across restart'
        source.seek(offset)
        return source.read().decode('utf-8', errors='replace')


def wait_desktop(path, offset, timeout=180):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        log = read_boot_log(path, offset) if path.exists() else ''
        if 'VEYNODE_DESKTOP_FAIL' in log:
            raise RuntimeError('Linux desktop fixture failed')
        if 'VEYNODE_DESKTOP_READY' in log:
            assert 'Device: virgl' in log and 'Accelerated: yes' in log
            return log
        time.sleep(.2)
    raise TimeoutError('Linux desktop readiness timeout for this boot')


def screenshot(client, vm):
    reply = client.tool('vm_screenshot', {'id': vm})
    if reply.get('isError') or reply['content'][0]['type'] != 'image':
        raise RuntimeError('actual MCP screenshot rejected')
    body = base64.b64decode(reply['content'][0]['data'], validate=True)
    metadata = json.loads(reply['content'][1]['text'])
    assert len(body) == metadata['byte_length'] > 0
    mime = reply['content'][0]['mimeType']
    assert (mime == 'image/png' and body.startswith(b'\x89PNG\r\n\x1a\n')) or (mime == 'image/jpeg' and body.startswith(b'\xff\xd8') and body.endswith(b'\xff\xd9'))
    assert metadata['width'] > 0 and metadata['height'] > 0
    assert 'internal_scanout_id' not in metadata
    decoded = decode_image(body, metadata['width'], metadata['height'])
    return {key: metadata[key] for key in ['frame_id', 'width', 'height', 'byte_length', 'mime_type'] if key in metadata} | {'sha256': hashlib.sha256(body).hexdigest(), 'independent_decode': decoded}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--mac-vm', required=True)
    parser.add_argument('--mac-helper-pid', type=int, required=True, help='operator-attributed unique new XPC helper from the Mac boot')
    parser.add_argument('--resource-probe', type=Path, default=ROOT/'.local/bin/resource-probe')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    state = args.state_dir.resolve(strict=True)
    output = args.output.absolute()
    output.parent.mkdir(parents=True, exist_ok=True)
    owned_id = 'mixed-linux-' + secrets.token_hex(8)
    assert args.mac_vm != owned_id and args.mac_helper_pid > 1
    control_socket = state/'control.sock'
    socket_identity = control_socket.stat()
    assert stat.S_ISSOCK(socket_identity.st_mode)
    report = {'passed': False, 'started_at_unix': time.time(), 'owned_vm_id': owned_id, 'checks': [],
              'scope': 'Actual one Apple macOS + one QEMU/HVF Linux, same pre-existing daemon. QEMU-to-Mac crash isolation only; no Mac fault injection, Windows, performance or release-quality claim.',
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'source_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'cleanup': {'owned_qemu_stopped': False}}
    # Reserve evidence exclusively before creating anything. Never overwrite another run.
    fd = os.open(output, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(fd, 'w') as evidence, tempfile.TemporaryDirectory(prefix='vm-mixed-', dir='/tmp') as temp:
        def save():
            evidence.seek(0); json.dump(report, evidence, indent=2); evidence.write('\n'); evidence.truncate(); evidence.flush(); os.fsync(evidence.fileno())
        def record(name, value):
            report['checks'].append({'name': name, 'evidence': value}); save(); print(name, flush=True)
        def call(method, payload=None):
            payload = payload or {}
            authorize(method, payload, owned_id)
            current = control_socket.stat()
            if (current.st_dev, current.st_ino) != (socket_identity.st_dev, socket_identity.st_ino):
                raise RuntimeError('controller socket changed; refusing to act on replacement daemon')
            result = subprocess.run([str(binary), 'call', '--state-dir', str(state), method, json.dumps(payload)], capture_output=True, text=True, timeout=40)
            if result.returncode:
                raise RuntimeError('controller rejected ' + method)  # no Guest bodies/path/error payloads in report
            return json.loads(result.stdout)
        def wait_state(vm, expected, timeout=25):
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                value = call('status', {'id': vm})
                if value['state'] in expected:
                    return value
                time.sleep(.1)
            raise TimeoutError('VM did not reach expected state')
        def mac_unchanged():
            value = call('status', {'id': args.mac_vm})
            assert value['state'] == 'running' and value['pid'] == mac_pid
            reply = call('guest_info', {'id': args.mac_vm})
            assert not reply.get('error') and not reply.get('result', {}).get('error')
            return {'state': value['state'], 'worker_pid': value['pid'], 'guest_info_replied': True}
        module_spec = importlib.util.spec_from_file_location('mixed_qemu_fixture', ROOT/'scripts/test-control-qemu.py')
        qemu = importlib.util.module_from_spec(module_spec); module_spec.loader.exec_module(qemu)
        clients = []; created = False; serial = None; mac_pid = None
        save()
        try:
            mac = call('status', {'id': args.mac_vm})
            assert mac['config']['guest'] == 'macos' and mac['config'].get('backend', 'apple-virtualization') == 'apple-virtualization'
            assert mac['state'] == 'running' and mac['config']['memory_mib'] == 4096
            mac_pid = mac['pid']
            table = process_table()
            assert table[args.mac_helper_pid]['executable'] == HELPER
            daemon_pid = table[mac_pid]['parent']
            caps = call('capabilities')
            assert caps['backends']['qemu-hvf']['registered'] is True
            assert caps['backends']['qemu-hvf']['capabilities']['screenshot'] is True
            assert caps['resource_admission']['active_assigned_memory_mib'] == 4096, 'requires only the Mac active in this daemon'
            assert caps['resource_admission']['remaining_budget_mib'] >= 1536
            record('attached_existing_mac_and_registered_qemu', {'mac': mac_unchanged(), 'daemon_pid': daemon_pid,
                   'mac_helper_pid': args.mac_helper_pid, 'helper_attribution': 'operator-provided unique pre/post Mac-start exact-executable delta; script checks executable, not independently causally attributed', 'admission': caps['resource_admission']})
            directory = Path(temp)
            initrd, fixture = qemu.fixture(directory)
            config = {'id': owned_id, 'backend': 'qemu-hvf', 'guest': 'linux', 'cpus': 2, 'memory_mib': 1536,
                      'kernel': str(qemu.MEDIA/'Image'), 'initrd': str(initrd), 'command_line': 'console=ttyAMA0 rdinit=/veynode/gpu-init', 'serial_log': str(directory/'linux.serial.log')}
            assert not any(row['config']['id'] == owned_id for row in call('list'))
            call('create', config); created = True
            report['fixture'] = fixture; save()
            call('start', {'id': owned_id}); running = wait_state(owned_id, {'running'})
            assert running['runtime']['capture_mode'] == 'persistent-console' and running['runtime']['guest_ready'] is None
            serial = Path(running['runtime']['serial_log'])
            wait_desktop(serial, 0)
            caps = call('capabilities'); assert caps['resource_admission']['active_assigned_memory_mib'] == 5632
            record('mixed_backend_real_desktops_ready', {'mac': mac_unchanged(), 'qemu_worker_pid': running['pid'], 'qemu_backend_pid': running['runtime']['backend_pid'], 'assigned_mib': 5632, 'linux_readiness': 'Guest serial oracle, not SPICE/QMP acknowledgement'})
            mac_client = qemu.MCP(binary, state); linux_client = qemu.MCP(binary, state); clients += [mac_client, linux_client]
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                a = pool.submit(screenshot, mac_client, args.mac_vm); b = pool.submit(screenshot, linux_client, owned_id)
                record('concurrent_actual_mcp_screenshots', {'macos': a.result(), 'linux': b.result()})
            table = process_table(); group = {pid: row for pid, row in table.items() if row['group'] == running['pid']}
            assert running['pid'] in group and running['runtime']['backend_pid'] in group and table[running['pid']]['parent'] == daemon_pid
            pids = [mac_pid, args.mac_helper_pid, *sorted(group)]
            assert len(set(pids)) == len(pids)
            sampled = subprocess.run([str(args.resource_probe.resolve(strict=True)), '10', '200', str(5632*1024*1024), *map(str, pids)], capture_output=True, text=True, timeout=10)
            assert sampled.returncode == 0, 'resource sampler failed'
            rows = [json.loads(line) for line in sampled.stdout.splitlines()]; assert len(rows) == 10
            assert all(row['host_physical_footprint_bytes'] == sum(p['physical_footprint_bytes'] for p in row['processes']) for row in rows)
            record('short_actual_process_ledger_samples', {'samples': rows, 'roles': {'mac_worker': mac_pid, 'mac_vm_helper': args.mac_helper_pid, 'qemu_process_group': sorted(group)}, 'limits': '2-second functional diagnostics; known process ledger sum, not total system RAM. Excludes unrelated/other service and kernel overhead; not a performance or Parallels comparison.'})
            ack = call('shutdown', {'id': owned_id}); assert ack['requested'] and not ack['completed']
            wait_state(owned_id, {'stopped'})
            log = serial.read_text(errors='replace'); assert 'VEYNODE_ACPI_POWER_BUTTON' in log and 'VEYNODE_INIT_NORMAL_SHUTDOWN' in log
            record('qemu_normal_shutdown_mac_survives', {'acpi_and_pid1_markers': True, 'mac': mac_unchanged()})
            restart_serial_offset = serial.stat().st_size
            call('start', {'id': owned_id}); restarted = wait_state(owned_id, {'running'})
            assert Path(restarted['runtime']['serial_log']) == serial
            wait_desktop(serial, restart_serial_offset)
            restarted_screenshot = screenshot(linux_client, owned_id)
            record('second_boot_desktop_ready_before_fault', {'serial_offset': restart_serial_offset, 'new_boot_ready_marker': True, 'linux_screenshot': restarted_screenshot, 'mac': mac_unchanged()})
            # Fault is confined to the newly created RAM-only QEMU worker. Never
            # signal the daemon, macOS worker, XPC helper, or an arbitrary PGID.
            table = process_table(); victim = restarted['pid']; victim_group = {pid for pid, row in table.items() if row['group'] == victim}
            assert victim not in {mac_pid, args.mac_helper_pid, daemon_pid} and victim > 1
            assert table[victim]['parent'] == daemon_pid and table[victim]['group'] == victim
            latest = call('status', {'id': owned_id}); assert latest['pid'] == victim and latest['state'] == 'running'
            assert latest['config']['backend'] == 'qemu-hvf' and latest['config']['id'] == owned_id
            os.kill(victim, signal.SIGKILL)
            wait_state(owned_id, {'crashed'})
            end = time.monotonic() + 10
            while time.monotonic() < end:
                present = process_table()
                if all(pid not in present or present[pid]['state'].startswith('Z') for pid in victim_group):
                    break
                time.sleep(.1)
            else:
                raise RuntimeError('owned QEMU worker group still executing after crash')
            record('owned_qemu_worker_crash_mac_survives', {'mac': mac_unchanged(), 'mac_screenshot': screenshot(mac_client, args.mac_vm), 'qemu_group_not_executing': True, 'qemu_owned_worker_pid': victim, 'fault_direction': 'QEMU to macOS only'})
            assert call('capabilities')['resource_admission']['active_assigned_memory_mib'] == 4096
            report['passed'] = True
        except Exception as error:
            report['error_type'] = type(error).__name__
            report['error'] = str(error)[:400]
        finally:
            for client in clients:
                try:
                    client.close(); assert client.p.returncode == 0
                except Exception:
                    report['passed'] = False; report['cleanup']['mcp_cleanup_failed'] = True
            if created:
                try:
                    value = call('status', {'id': owned_id})
                    if value['state'] not in {'stopped', 'crashed'}:
                        call('force_stop', {'id': owned_id, 'confirm': owned_id})
                    assert call('status', {'id': owned_id})['state'] in {'stopped', 'crashed'}
                    report['cleanup']['owned_qemu_stopped'] = True
                except Exception:
                    report['passed'] = False; report['cleanup']['owned_qemu_cleanup_unconfirmed'] = True
            if serial and serial.exists():
                # Keep the diagnostic fixture log alongside evidence, never Guest screen/file bodies.
                log_path = output.with_suffix('.linux.serial.log')
                try:
                    log_fd = os.open(log_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
                    with os.fdopen(log_fd, 'wb') as log_file:
                        log_file.write(serial.read_bytes())
                except OSError:
                    report['passed'] = False; report['cleanup']['serial_evidence_preservation_failed'] = True
            report['cleanup']['stopped_registry_entry_retained'] = created
            report['finished_at_unix'] = time.time(); save()
    print(('PASS: ' if report['passed'] else 'FAIL: ') + str(output))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
