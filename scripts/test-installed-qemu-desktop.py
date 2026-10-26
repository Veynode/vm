#!/usr/bin/env python3
"""Actual installed QEMU desktop acceptance against an already running controller.

Only NEW fixture VM registrations and explicitly supplied disposable clone disks
are used. The fixture's diagnostic boot marker is not a Guest command API. Source
and read-only base are hashed, never booted or mounted. No daemon is started here.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import stat
import subprocess
import time
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
TRANSPORT = 'virtio-serial-private-observer-v1'
MAX_LOG = 32 * 1024 * 1024


def require(value, message):
    if not value:
        raise AssertionError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(4 * 1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def boot_evidence(data):
    """Use complete records only; a partial concurrently written line is pending."""
    lines = data.replace(b'\r\n', b'\n').split(b'\n')[:-1]
    boot, cases, summary = [], [], []
    for line in lines:
        for prefix, target in [(b'VEYNODE_INSTALLED_BOOT ', boot),
                               (b'XRENDER_CASE ', cases), (b'XRENDER_SUMMARY ', summary)]:
            if line.startswith(prefix):
                target.append(json.loads(line[len(prefix):]))
    if b'VEYNODE_INSTALLED_GRAPHICS_READY=15' not in lines:
        return None
    require(len(boot) == 1, 'missing or repeated installed boot identity')
    identity = boot[0]
    require(re.fullmatch(r'[0-9a-f-]{36}', identity.get('boot_id', '')), 'invalid actual boot ID')
    require(identity.get('root_fstype') == 'ext4' and identity.get('root_device') == '/dev/vda',
            'Guest did not boot its installed ext4 disk')
    require(len(cases) == 15 and len({case['name'] for case in cases}) == 15,
            'pixel cases are missing or duplicated')
    require(all(case.get('passed') is True and case.get('mismatched_pixels') == 0 for case in cases),
            'actual pixel comparison failed')
    require(summary == [{'cases': 15, 'failed': 0, 'passed': True}], 'pixel summary mismatch')
    return {'boot': identity, 'pixel_cases': cases, 'pixel_summary': summary[0]}


class MCP:
    def __init__(self, binary, state):
        self.p = subprocess.Popen([str(binary), 'mcp', '--state-dir', str(state),
            '--allow-lifecycle', '--allow-execute', '--allow-files'], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.sequence, self.buffer = 0, bytearray()
        reply = self.rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
            'clientInfo': {'name': 'installed-qemu-desktop-acceptance', 'version': '1'}})
        require(reply.get('result', {}).get('protocolVersion') == '2025-06-18', 'MCP initialize failed')
        self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def send(self, value):
        self.p.stdin.write(json.dumps(value).encode() + b'\n')
        self.p.stdin.flush()

    def rpc(self, method, params):
        self.sequence += 1
        self.send({'jsonrpc': '2.0', 'id': self.sequence, 'method': method, 'params': params})
        deadline = time.monotonic() + 40
        while b'\n' not in self.buffer:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and select.select([self.p.stdout], [], [], remaining)[0], 'MCP timeout')
            block = os.read(self.p.stdout.fileno(), 65536)
            require(block, 'MCP EOF without reply')
            self.buffer.extend(block)
            require(len(self.buffer) <= 24 * 1024 * 1024, 'MCP response bound exceeded')
        line, _, rest = self.buffer.partition(b'\n')
        self.buffer = bytearray(rest)
        reply = json.loads(line)
        require(reply.get('id') == self.sequence, 'MCP response ID mismatch')
        return reply

    def tool(self, name, args, denied=False, raw=False):
        reply = self.rpc('tools/call', {'name': 'vm_' + name, 'arguments': args})
        failed = 'error' in reply or reply.get('result', {}).get('isError') is True
        require(failed == denied, f'unexpected {name} result: {reply}')
        if raw or denied:
            return reply
        content = reply['result']['content']
        require(len(content) == 1 and content[0].get('type') == 'text', 'invalid tool content')
        return json.loads(content[0]['text'])

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=42)
            require(self.p.returncode == 0, 'MCP session cleanup not confirmed')
        finally:
            if self.p.poll() is None:
                self.p.kill()
                self.p.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'state', 'config-a', 'config-b', 'source', 'base', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--image-binary', type=Path, default=ROOT / 'target/release/vm-image')
    parser.add_argument('--apple-binary', type=Path,
                        default=ROOT / '.local/VeynodeVM-files-stage8.app/Contents/MacOS/vm-apple')
    args = parser.parse_args()
    for name, value in vars(args).items():
        setattr(args, name, value.resolve())
    require(shutil.which('ffprobe'), 'independent PNG decoder ffprobe required')
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    evidence = {'started_at_unix': time.time(), 'checks': [], 'passed': False,
        'scope': 'actual installed ARM Linux, private observer-only serial, native QEMU capture and MCP',
        'limits': ['fixture boot markers are diagnostic writes, not production command/file integration',
                   'no application compatibility, sustained performance or Parallels comparison',
                   'sequential two-clone test, not concurrent multi-VM acceptance']}
    current, client = None, None

    def record(name, value):
        evidence['checks'].append({'name': name, 'evidence': value})
        (args.output / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print(name, flush=True)

    def local(method, payload):
        reply = subprocess.run([str(args.binary), 'call', '--state-dir', str(args.state), method,
            json.dumps(payload)], capture_output=True, text=True, timeout=42)
        require(reply.returncode == 0, f'local {method}: {reply.stderr}')
        return json.loads(reply.stdout)

    try:
        configs = [json.loads(path.read_text()) for path in (args.config_a, args.config_b)]
        disks = [Path(config['disk']).resolve() for config in configs]
        paths = [args.source, args.base, *disks]
        require(len({(p.stat().st_dev, p.stat().st_ino) for p in paths}) == 4, 'disk identities alias')
        require(all(p.is_relative_to(ROOT / '.local') and stat.S_ISREG(p.stat().st_mode) for p in paths),
                'only repository-created local fixture disks accepted')
        require(args.base.stat().st_mode & 0o222 == 0, 'base must be read-only')
        for config in configs:
            require(config.get('backend') == 'qemu-hvf' and config.get('guest') == 'linux', 'wrong backend')
            require('veynode.fixture_marker=' not in config.get('command_line', ''), 'marker already configured')
        before = {str(path): digest(path) for path in (args.source, args.base)}
        record('immutable source/base preflight', {'sha256': before, 'controller_sha256': digest(args.binary),
            'disk_identities': [{'path': str(p), 'device': p.stat().st_dev, 'inode': p.stat().st_ino,
                                 'size': p.stat().st_size} for p in paths]})
        known = {entry['config']['id'] for entry in local('list', {})}
        token = 'clone-a-' + uuid.uuid4().hex
        boots = []
        for index, (template, suffix) in enumerate([(configs[0], 'write'), (configs[0], 'reboot'), (configs[1], 'peer')]):
            vm = template['id'] + '-' + suffix
            require(vm not in known and len(vm) <= 64, 'test VM ID exists or is too long')
            config = dict(template, id=vm, serial_log=str(args.output / (suffix + '.serial.log')))
            if index == 0:
                config['command_line'] += ' veynode.fixture_marker=' + token
            (args.output / (suffix + '.config.json')).write_text(json.dumps(config, indent=2) + '\n')
            local('create', config)
            client = MCP(args.binary, args.state)
            current = vm
            start = client.tool('start', {'id': vm})
            record(suffix + ' actual start', start)
            log = Path(config['serial_log'])
            deadline, actual = time.monotonic() + 180, None
            while time.monotonic() < deadline:
                status = local('status', {'id': vm})
                require(status['state'] not in ('stopped', 'crashed', 'error'), 'Guest exited before readiness')
                if log.exists():
                    require(log.stat().st_size <= MAX_LOG, 'serial log exceeds bound')
                    actual = boot_evidence(log.read_bytes())
                    if actual:
                        break
                time.sleep(.5)
            require(actual is not None, 'installed Guest/pixel readiness deadline')
            provenance = status['runtime']['disk_provenance']
            disk_stat = Path(config['disk']).stat()
            require(provenance.get('device') == disk_stat.st_dev and provenance.get('inode') == disk_stat.st_ino
                    and provenance.get('descriptor_backed') is True
                    and provenance.get('qmp_fdsets_verified_at_start') is True,
                    'running backend is not bound to the selected disk inode')
            record(suffix + ' actual runtime disk binding', provenance)
            boots.append(actual['boot'])
            require(len({b['boot_id'] for b in boots}) == len(boots), 'reused actual boot identity')
            record(suffix + ' installed disk and actual pixels', actual)
            if index == 0:
                # Deliberately false stopped assertion is the negative control:
                # an independent image process must still enforce the inode lock.
                # Use a hardlink to prove this is not a same-path string check.
                alias = args.output / 'active-clone-hardlink.raw'
                os.link(disks[0], alias)
                try:
                    rejected = subprocess.run([str(args.image_binary), 'import', '--id', 'active-denied',
                        '--source', str(alias), '--source-stopped', '--store', str(args.output / 'rejected-import')],
                        text=True, capture_output=True, timeout=30)
                    require(rejected.returncode != 0 and 'active VM disks cannot be imported' in rejected.stderr,
                            'independent image import did not reject active disk inode')
                    require(not (args.output / 'rejected-import/bases/active-denied').exists(),
                            'rejected import published an image')
                    record('independent vm-image rejects active disk through hardlink',
                           {'exit_code': rejected.returncode, 'error': rejected.stderr,
                            'image_binary_sha256': digest(args.image_binary)})
                    # The Swift worker locks before building its VM config. An
                    # intentionally absent kernel also prevents any VZ Guest
                    # creation if the expected busy rejection regresses.
                    vz_config = args.output / 'vz-lock-negative.json'
                    vz_config.write_text(json.dumps({'id': 'qemu-vz-lock-negative', 'guest': 'linux',
                        'cpus': 1, 'memory_mib': 512, 'disk': str(alias), 'window': False}))
                    vz = subprocess.run([str(args.apple_binary), 'run', '--config', str(vz_config)],
                        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=5)
                    events = [json.loads(line) for line in vz.stdout.splitlines() if line]
                    require(vz.returncode == 1 and len(events) == 1 and events[0].get('state') == 'error'
                            and 'Writable disk is already in use or cannot be locked' in events[0].get('message', ''),
                            'actual Swift backend did not reject the active QEMU disk lock')
                    require(local('status', {'id': vm})['state'] == 'running', 'lock checks disrupted QEMU')
                    record('actual Swift worker rejects active QEMU hardlink before VZ creation',
                           {'events': events, 'exit_code': vz.returncode,
                            'apple_binary_sha256': digest(args.apple_binary), 'guest_started': False})
                finally:
                    alias.unlink()
            # The diagnostic READY line precedes OpenRC starting the serial
            # service. Poll bounded readiness; never reconnect/restart a fenced
            # transport or treat a failing RPC as a usable Guest capability.
            info, info_deadline = None, time.monotonic() + 15
            while time.monotonic() < info_deadline:
                reply = client.rpc('tools/call', {'name': 'vm_guest_info', 'arguments': {'id': vm}})
                result = reply.get('result', {})
                if result.get('isError') is False:
                    info = json.loads(result['content'][0]['text'])['result']
                    break
                require('guest integration unavailable' in json.dumps(reply),
                        'Guest info failed for a reason other than pending readiness')
                time.sleep(.25)
            require(info is not None, 'private Guest service readiness deadline')
            require(info['transport'] == TRANSPORT and info.get('transport_reconnect') is False,
                    'wrong private observer transport')
            require(info['os'] == 'linux' and info['arch'] == 'arm64', 'wrong Guest platform')
            require(all(info['capabilities'].get(k) is False for k in
                    ('exec', 'async_exec', 'files', 'shutdown', 'desktop', 'descendant_containment')),
                    'observer-only Guest advertises side effects')
            record(suffix + ' actual MCP Guest info', info)
            # The client holds execute and files permission, so these must fail by
            # backend capability, not merely by missing permission or unknown tool.
            for method, payload in [('exec_start', {'argv': ['/bin/true'], 'timeout_ms': 1000}),
                    ('file_upload_begin', {'path': 'must-not-exist', 'size': 0,
                     'sha256': hashlib.sha256(b'').hexdigest()})]:
                denied = client.tool(method, {'id': vm, **payload}, denied=True)
                require('unsupported' in json.dumps(denied).lower(), 'denial did not identify unsupported capability')
                record(suffix + ' ' + method + ' explicitly unsupported', denied)
            image = client.tool('screenshot', {'id': vm}, raw=True)['result']['content']
            pictures = [part for part in image if part['type'] == 'image']
            metadata = [json.loads(part['text']) for part in image if part['type'] == 'text']
            require(len(pictures) == 1 and pictures[0]['mimeType'] == 'image/png' and len(metadata) == 1,
                    'invalid real screenshot content')
            png = args.output / (suffix + '.png')
            png.write_bytes(base64.b64decode(pictures[0]['data'], validate=True))
            decoded = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_entries',
                'stream=codec_name,width,height', '-of', 'json', str(png)], text=True, timeout=15))
            require(decoded['streams'] == [{'codec_name': 'png', 'width': config['width'], 'height': config['height']}],
                    'independent screenshot decode/geometry mismatch')
            record(suffix + ' actual screenshot independently decoded', {'metadata': metadata[0],
                'sha256': digest(png), 'decode': decoded})
            shutdown = client.tool('shutdown', {'id': vm})
            require(shutdown.get('requested') is True and shutdown.get('completed') is False
                    and shutdown.get('mechanism') == 'qmp-system-powerdown',
                    'shutdown must be a Guest power-button request, not forced termination')
            record(suffix + ' normal shutdown requested', shutdown)
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                status = local('status', {'id': vm})
                if status['state'] in ('stopped', 'crashed', 'error'):
                    break
                time.sleep(.25)
            require(status['state'] == 'stopped' and status.get('exit_code') == 0, 'normal Guest shutdown not verified')
            require(b'VEYNODE_INSTALLED_ACPI_POWER_BUTTON' in log.read_bytes(),
                    'Guest ACPI handler did not record normal poweroff')
            record(suffix + ' actual normal exit', status)
            current = None
            client.close()
            client = None
            record(suffix + ' MCP EOF cleanup exit zero', True)
        require(boots[0]['marker_after'] == token and boots[1]['marker_before'] == token
                and boots[1]['marker_after'] == token, 'clone A marker did not persist across real boot')
        require(boots[2]['marker_before'] == boots[0]['marker_before']
                and boots[2]['marker_after'] == boots[0]['marker_before']
                and boots[2]['marker_after'] != token, 'clone B sees A writes or changed unexpectedly')
        record('actual private write survives reboot and is absent from peer clone', boots)
        after = {path: digest(Path(path)) for path in before}
        require(before == after, 'source or read-only base changed')
        record('complete source/base SHA256 unchanged', after)
        evidence['passed'] = True
    except BaseException as error:
        evidence['error'] = str(error)
        raise
    finally:
        if current is not None:
            try:
                status = local('status', {'id': current})
                if status['state'] not in ('stopped', 'crashed', 'error'):
                    evidence['forced_failure_cleanup'] = local('force_stop', {'id': current, 'confirm': current})
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        status = local('status', {'id': current})
                        if status['state'] in ('stopped', 'crashed', 'error'):
                            break
                        time.sleep(.1)
                    evidence['forced_cleanup_terminal_state'] = status
                    require(status['state'] in ('stopped', 'crashed', 'error'), 'forced cleanup remains unconfirmed')
            except Exception as error:
                evidence['cleanup_error'] = str(error)
        if client is not None:
            try:
                client.close()
            except Exception as error:
                evidence['client_cleanup_error'] = str(error)
        evidence['finished_at_unix'] = time.time()
        (args.output / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')


class BootRecords(unittest.TestCase):
    def valid(self):
        lines = ['VEYNODE_INSTALLED_BOOT ' + json.dumps({'boot_id': str(uuid.uuid4()),
                 'root_fstype': 'ext4', 'root_device': '/dev/vda'})]
        lines += ['XRENDER_CASE ' + json.dumps({'name': str(i), 'passed': True, 'mismatched_pixels': 0}) for i in range(15)]
        lines += ['XRENDER_SUMMARY {"cases":15,"failed":0,"passed":true}', 'VEYNODE_INSTALLED_GRAPHICS_READY=15']
        return ('\n'.join(lines) + '\n').encode()

    def test_incomplete_readiness_is_pending(self):
        data = self.valid()
        self.assertIsNone(boot_evidence(data[:-1]))
        self.assertIsNotNone(boot_evidence(data))

    def test_false_positive_summaries_are_rejected(self):
        data = self.valid()
        for bad in [data.replace(b'"name": "14"', b'"name": "13"'),
                    data.replace(b'"mismatched_pixels": 0', b'"mismatched_pixels": 1', 1),
                    data.replace(b'"root_fstype": "ext4"', b'"root_fstype": "tmpfs"')]:
            with self.assertRaises(AssertionError):
                boot_evidence(bad)


if __name__ == '__main__':
    if '--self-test' in os.sys.argv:
        unittest.main(argv=[os.sys.argv[0]])
    else:
        main()
