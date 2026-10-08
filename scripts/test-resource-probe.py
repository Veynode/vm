#!/usr/bin/env python3
"""Calibrate actual macOS ledger/CPU sampling, optionally sample one real Linux VM.
Does not benchmark graphics or compare products; never opens an existing VM disk.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from native_client import NativeClient

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / '.local/bin/resource-probe'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--real', action='store_true')
    parser.add_argument('--output', type=Path, default=ROOT / '.local/evidence/resource-probe.json')
    args = parser.parse_args()
    subprocess.run([str(ROOT / 'native/tools/build-resource-probe.sh')], check=True)
    result = {'started_at_unix': time.time(), 'passed': False, 'checks': [],
              'scope': 'measurement correctness and optional real VM worker; no desktop/Parallels comparison'}

    def sample(pid, count=3, assigned=0):
        pids = pid if isinstance(pid, list) else [pid]
        command = [str(BIN), str(count), '100', str(assigned), *map(str, pids)]
        response = subprocess.run(command, capture_output=True, text=True, timeout=10)
        assert response.returncode == 0, response.stderr
        rows = [json.loads(line) for line in response.stdout.splitlines()]
        assert len(rows) == count
        for index, row in enumerate(rows):
            assert row['sample_index'] == index
            assert row['guest_assigned_bytes'] == assigned
            assert row['host_physical_footprint_bytes'] == sum(p['physical_footprint_bytes'] for p in row['processes'])
            assert row['host_physical_footprint_bytes'] > 0
            assert row['host_cpu_percent'] >= 0
            if index:
                assert row['mach_tick'] > rows[index-1]['mach_tick']
                assert row['processes'][0]['start_mach_tick'] == rows[index-1]['processes'][0]['start_mach_tick']
        return rows

    child = subprocess.Popen([sys.executable, '-u', '-c', '''
import sys,time
print('ready',flush=True)
for line in sys.stdin:
    if line.strip() == 'allocate':
        allocation=bytearray(128*1024*1024)
        for i in range(0,len(allocation),4096): allocation[i]=123
        print('allocated',flush=True)
    elif line.strip() == 'busy':
        print('busy',flush=True)
        until=time.monotonic()+3
        while time.monotonic()<until: pass
        print('idle',flush=True)
'''], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == 'ready'
        before = sample(child.pid)
        child.stdin.write('allocate\n'); child.stdin.flush()
        assert child.stdout.readline().strip() == 'allocated'
        after = sample(child.pid)
        growth = after[-1]['host_physical_footprint_bytes']-before[-1]['host_physical_footprint_bytes']
        assert 96*1024*1024 < growth < 192*1024*1024, growth
        result['checks'].append({'name': 'actual touched 128 MiB allocation is reflected in kernel footprint',
                                 'growth_bytes': growth, 'before': before, 'after': after})
        child.stdin.write('busy\n'); child.stdin.flush()
        assert child.stdout.readline().strip() == 'busy'
        busy = sample(child.pid, count=10)
        cpu = sum(row['host_cpu_percent'] for row in busy)/len(busy)
        # Broad correctness calibration, not a host performance threshold.
        # A mistaken ns/Mach-tick conversion on this Mac would produce about 2.4%.
        assert 30 < cpu < 130, cpu
        result['checks'].append({'name': 'one busy thread CPU unit calibration; 100% means one core',
                                 'mean_cpu_percent': cpu, 'samples': busy})
        duplicate = subprocess.run([str(BIN), '1', '100', '0', str(child.pid), str(child.pid)], capture_output=True, text=True)
        assert duplicate.returncode == 2 and not duplicate.stdout and 'duplicate PID' in duplicate.stderr
        child.terminate(); child.wait(timeout=5)
        gone = subprocess.run([str(BIN), '1', '100', '0', str(child.pid)], capture_output=True, text=True)
        assert gone.returncode == 1 and not gone.stdout, gone
        result['checks'].append({'name': 'duplicate and exited targets rejected without partial aggregate', 'passed': True})
        transient = subprocess.Popen([sys.executable, '-u', '-c',
            "import time; print('ready',flush=True); time.sleep(.35)"], stdout=subprocess.PIPE, text=True)
        try:
            assert transient.stdout.readline().strip() == 'ready'
            interrupted = subprocess.run([str(BIN), '10', '100', '0', str(transient.pid)],
                                         capture_output=True, text=True, timeout=5)
            assert interrupted.returncode == 1, interrupted
            prefix = [json.loads(line) for line in interrupted.stdout.splitlines()]
            assert len(prefix) < 10
            assert all(row['host_physical_footprint_bytes'] > 0 for row in prefix)
            result['checks'].append({'name': 'target exit during sampling fails; no zero-filled success',
                                     'complete_prefix_samples': len(prefix), 'error': interrupted.stderr.strip()})
        finally:
            if transient.poll() is None:
                transient.terminate()
            transient.wait(timeout=5)
        if args.real:
            helper_path = '/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine'
            def helper_pids():
                listing = subprocess.check_output(['ps', '-axo', 'pid=,comm='], text=True)
                return {int(parts[0]) for line in listing.splitlines()
                        if len(parts := line.strip().split(maxsplit=1)) == 2 and parts[1] == helper_path}
            with tempfile.TemporaryDirectory(prefix='vm-resource-') as temp:
                config = json.loads((ROOT/'.local/run/linux-agent.json').read_text())
                assert not config.get('disk'), 'only diskless disposable fixture allowed'
                config.update(id='resource-'+str(os.getpid()), cpus=1, memory_mib=512,
                              serial_log=str(Path(temp)/'serial.log'), window=False)
                path = Path(temp)/'config.json'
                path.write_text(json.dumps(config))
                before_helpers = helper_pids()
                with NativeClient(ROOT/'.local/bin/vm-apple', path) as vm:
                    info = vm.ready()
                    new_helpers = helper_pids()-before_helpers
                    assert len(new_helpers) == 1, 'ambiguous helper attribution; avoid concurrent VM starts'
                    helper = new_helpers.pop()
                    rows = sample([vm.process.pid, helper], count=10, assigned=512*1024*1024)
                    assert all(row['processes'][1]['physical_footprint_bytes'] > row['processes'][0]['physical_footprint_bytes'] for row in rows)
                    result['checks'].append({'name': 'actual Apple VZ Linux worker and independently hosted VM helper sampled',
                        'roles': {'worker': vm.process.pid, 'vm_helper': helper},
                        'helper_attribution': 'unique new exact executable across isolated VM start; must disappear on stop',
                        'guest_info': info, 'samples': rows,
                        'limits': 'One diskless 512 MiB idle Linux VM. Sum of two process ledgers, not total system RAM delta. Other helpers/kernel services, encoder/UI and host overhead excluded; no general concurrent helper-discovery claim.'})
                assert vm.process.poll() is not None
                deadline = time.monotonic()+5
                while helper in helper_pids() and time.monotonic() < deadline:
                    time.sleep(.05)
                assert helper not in helper_pids(), 'helper did not exit with owned VM; attribution unverified'
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        if child.poll() is None:
            child.terminate(); child.wait(timeout=5)
        result['finished_at_unix'] = time.time()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, ensure_ascii=False)+'\n')
    print('PASS: '+str(args.output))


if __name__ == '__main__':
    main()
