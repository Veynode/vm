#!/usr/bin/env python3
"""Actual installed Linux synchronous MCP cancellation; one VM <= 1 GiB, no fake Guest."""
import argparse
import hashlib
import json
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
CONTROL = ROOT / 'target/debug/vm-control'


class MCP:
    def __init__(self, state, scopes=None):
        self.process = subprocess.Popen([str(CONTROL), 'mcp', '--state-dir', str(state),
                                        *['--allow-'+scope for scope in (['lifecycle','execute'] if scopes is None else scopes)]], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.number = 0
        self.submit('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                  'clientInfo': {'name': 'sync-cancellation-validator', 'version': '1'}})
        assert self.reply()['result']['protocolVersion'] == '2025-06-18'
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.process.stdin.flush()

    def submit(self, method, params):
        self.number += 1
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': self.number, 'method': method, 'params': params}) + '\n')
        self.process.stdin.flush()

    def reply(self, timeout=40):
        assert select.select([self.process.stdout], [], [], timeout)[0], 'MCP response timeout'
        reply = json.loads(self.process.stdout.readline())
        assert reply['id'] == self.number, reply
        return reply

    def begin_tool(self, name, arguments):
        self.submit('tools/call', {'name': name, 'arguments': arguments})

    def finish_tool(self, denied=False, timeout=40):
        reply = self.reply(timeout)
        failed = 'error' in reply or reply.get('result', {}).get('isError')
        assert bool(failed) == denied, reply
        return reply if denied else json.loads(reply['result']['content'][0]['text'])

    def tool(self, name, arguments, denied=False):
        self.begin_tool(name, arguments)
        return self.finish_tool(denied)

    def close(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()
        if self.process.poll() is None:
            self.process.wait(timeout=10)


def main():
    global CONTROL
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=CONTROL)
    parser.add_argument('--backend', type=Path, default=ROOT / '.local/bin/vm-apple')
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=ROOT / '.local/evidence/control-sync-cancel.json')
    args = parser.parse_args()
    CONTROL = args.binary.resolve(strict=True)
    assert not args.output.exists() and not args.output.with_suffix('.failed.json').exists(), 'refusing evidence overwrite'
    config = json.loads(args.config.read_text())
    assert Path(config['disk']).resolve().is_relative_to(ROOT / '.local/vms'), 'only repository-created fixture disks'
    assert config['memory_mib'] <= 1024, 'one VM at most 1 GiB'
    config.update(id='sync-cancel-validation', window=False)
    evidence = {'started_at_unix': time.time(), 'checks': [], 'controller_sha256': hashlib.sha256(CONTROL.read_bytes()).hexdigest(), 'scope': 'Actual installed Linux, one VM <=1 GiB, MCP + daemon cancellation'}
    def record(name, value):
        evidence['checks'].append({'name': name, 'evidence': value})
    with tempfile.TemporaryDirectory(prefix='vm-sync-cancel-') as directory:
        state = Path(directory) / 'state'
        config['serial_log'] = str(Path(directory) / 'serial.log')
        daemon = subprocess.Popen([str(CONTROL), 'daemon', '--state-dir', str(state),
                                   '--backend', str(args.backend.resolve(strict=True))], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        clients = []
        def local(method, value):
            return json.loads(subprocess.check_output([str(CONTROL), 'call', '--state-dir',
                                                      str(state), method, json.dumps(value)], text=True, timeout=40))
        def client():
            instance = MCP(state)
            clients.append(instance)
            return instance
        def tool(instance, name, **values):
            return instance.tool(name, {'id': config['id'], **values})
        def execute(instance, argv, timeout_ms=5000):
            return tool(instance, 'vm_exec', argv=argv, timeout_ms=timeout_ms)['result']
        def command():
            stem = '/home/veynode/sync-' + uuid.uuid4().hex
            ready, late = stem + '-ready', stem + '-late'
            argv = ['/bin/sh', '-c', 'printf ready > "$1"; (sleep 3; printf leaked > "$2") & wait', 'test', ready, late]
            return argv, ready, late
        def verify_markers(instance, ready, late):
            reply = execute(instance, ['/bin/sh', '-c', 'test -f "$1" && test ! -e "$2"', 'verify', ready, late])
            assert reply['exit_code'] == 0, reply
            assert execute(instance, ['/bin/rm', '-f', ready, late])['exit_code'] == 0
        try:
            for _ in range(100):
                if (state / 'control.sock').exists():
                    break
                assert daemon.poll() is None
                time.sleep(.05)
            local('create', config)
            life, owner = client(), client()
            tool(life, 'vm_start')
            for _ in range(100):
                try:
                    info = tool(life, 'vm_guest_info')['result']
                    break
                except AssertionError:
                    time.sleep(.1)
            else:
                raise AssertionError('Guest service unavailable')
            assert info['capabilities']['async_exec'], info
            record('real installed Guest identity and capabilities', info)
            normal = execute(owner, ['/bin/sh', '-c', 'printf "你好 ✓"; exit 7'])
            assert normal['stdout'] == '你好 ✓' and normal['exit_code'] == 7 and not normal['cancelled'], normal
            record('synchronous Unicode and nonzero exit preserve API', normal)
            timed = execute(owner, ['/bin/sleep', '10'], 150)
            assert timed['timed_out'] and not timed['cancelled'], timed
            record('synchronous timeout retains actual terminal semantics', timed)
            for case in ['stdin-eof', 'broken-stdout']:
                owner = client()
                argv, ready, late = command()
                owner.begin_tool('vm_exec', {'id': config['id'], 'argv': argv, 'timeout_ms': 10000})
                time.sleep(.6)
                started = time.monotonic()
                if case == 'stdin-eof':
                    owner.process.stdin.close()
                else:
                    owner.process.stdout.close()
                assert owner.process.wait(timeout=4) == (0 if case == 'stdin-eof' else 1)
                terminal = {'client_exit_code': owner.process.returncode}
                elapsed = time.monotonic() - started
                assert elapsed < 4, elapsed
                verifier = client()
                time.sleep(3.1)
                verify_markers(verifier, ready, late)
                record(case + ' stops synchronous command and ordinary descendant',
                       {'terminal': terminal, 'elapsed_after_action_seconds': elapsed, 'ready_present_late_absent': True})

            argv, ready, late = command()
            raw = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            raw.connect(str(state / 'control.sock'))
            raw.sendall((json.dumps({'method': 'exec', 'arguments': {'id': config['id'],
                         'argv': argv, 'timeout_ms': 10000}}) + '\n').encode())
            time.sleep(.6)
            raw.close()
            time.sleep(3.1)
            verify_markers(verifier, ready, late)
            record('direct daemon Unix client disconnect stops synchronous descendant', True)
            assert tool(life, 'vm_shutdown', mode='guest')['result']['requested']
            for _ in range(150):
                if local('status', {'id': config['id']})['state'] == 'stopped':
                    break
                time.sleep(.1)
            else:
                raise AssertionError('Guest normal shutdown did not finish')
            record('normal lifecycle Guest shutdown', True)
            evidence['passed'] = True
        except Exception as error:
            evidence.update(passed=False, error=str(error))
            raise
        finally:
            for instance in clients:
                try:
                    instance.close()
                except Exception as error:
                    evidence.setdefault('client_cleanup_errors', []).append(str(error))
            try:
                if local('status', {'id': config['id']})['state'] != 'stopped':
                    evidence['forced_failure_cleanup'] = local('force_stop', {'id': config['id'], 'confirm': config['id']})
            finally:
                daemon.terminate()
                daemon.wait(timeout=10)
                evidence['finished_at_unix'] = time.time()
                args.output.parent.mkdir(parents=True, exist_ok=True)
                output = args.output if evidence.get('passed') else args.output.with_suffix('.failed.json')
                output.write_text(json.dumps(evidence, indent=2) + '\n')
                print(output)


if __name__ == '__main__':
    main()
