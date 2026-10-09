#!/usr/bin/env python3
"""Two installed Linux VMs under ONE existing empty dedicated daemon (journal v2 by default; explicit v3 opt-in).

Operator-only actual acceptance; this script never starts/stops a daemon or uses
reconcile_execution. A is a stopped disposable exec-v2 clone. B is an unused,
actually maintained prepare-qemu-guest-crash fixture. Both boot headless, B first.
Only these newly registered IDs may be force-stopped on failure. No Mac actions.

Required: --binary --worker --state --a-config --a-base --b-fixture
          --b-maintenance (actual result.json) --output (new directory).
No private one-shot nonce, command body or image bytes are written to result.json.
Serial evidence remains private and MUST be sanitized before publication.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import select
import stat
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / file)
    value = importlib.util.module_from_spec(spec); spec.loader.exec_module(value); return value


base = module('peer_baseline', 'test-installed-qemu-exec.py')
process = module('peer_process', 'test-qemu-exec-guardian.py')
witness = module('peer_witness', 'test-qemu-exec-witness-recovery.py')
crash = module('peer_crash', 'test-qemu-guest-crash.py')
oracle = module('peer_oracle', 'test-installed-qemu-exec-lost-ack.py')
require, digest = base.require, base.digest


def projection(journal, identity, version=2):
    """Validate the selected strict schema, then compare ONLY the exact VM incarnation."""
    witness.validate_journal(json.dumps(journal).encode(), version)
    return {key: [row for row in journal[key] if all(row[k] == identity[k]
             for k in ('vm', 'worker_instance'))] for key in ('scopes', 'launches', 'completed')}


def active_projection(journal, identity, version=2):
    result = projection(journal, identity, version)
    require(result['launches'] == [identity] and not result['completed']
            and len(result['scopes']) == 1, 'exact active VM launch/scope missing')
    return result


def preserve_peer(before, journal, identity, version=2):
    require(projection(journal, identity, version) == before, 'peer incarnation journal changed')


def release_acknowledged(value):
    # MCP.tool unwraps text JSON only. Controller release is direct, unlike
    # Guest exec methods whose text payload still contains a result envelope.
    require(isinstance(value, dict) and value.get('released') is True,
            'A scope release not acknowledged')


def normal_proof_ready(status, journal, identity, version=2):
    # Poll pending status/journal publication. A terminal error is not success;
    # caller saves every raw pair BEFORE evaluating this readiness predicate.
    value = projection(journal, identity, version)
    return (status.get('state') == 'stopped' and type(status.get('exit_code')) is int
            and status['exit_code'] == 0
            and status.get('execution_reconciliation_status') != 'unconfirmed'
            and not status.get('execution_recovery_required', False)
            and not status.get('execution_reconciliation_required', False)
            and value == {'scopes': [], 'launches': [], 'completed': [identity]})


def pending_witness_intact(state, identity):
    """Read exact pending file metadata/content; not a fresh QMP/lock probe."""
    value=witness.witness_from_runtime({'execution_witness':{**identity,'qmp_fdsets_verified_at_start':True}},
        identity['vm'],identity['worker_instance'],state)
    require(value==identity, 'pending B witness changed or was collected')


def completed_gc_ready(state, identity):
    try: witness.require_completed_gc(state,identity);return True
    except AssertionError as error:
        # A missing/unsafe journal directory is NEVER interpreted as GC success.
        if str(error)=='completed witness metadata was not collected':return False
        raise


def same_live_chain(pins, observations):
    require(len(pins) == len(observations) == 3 and all(
        process.same_incarnation(pin, now) and now['executable'] == pin['executable']
        and now['state'] not in (4, 5) and now['ppid'] == pin['ppid']
        for pin, now in zip(pins, observations)), 'survivor VM chain changed or stopped')


def increasing(before, after):
    require(after['tick_bytes'] > before['tick_bytes']
            and after['guest_elapsed_seconds'] > before['guest_elapsed_seconds'], 'survivor stopped making progress')


def bounded_guest_info(client, vm, timeout=3):
    """One read-only request on the lifecycle client, no retry or reconnect.

    Timeout leaves this client's pending response uncertain; only EOF cleanup
    follows. Never reuses it for another request after this diagnostic.
    """
    client.sequence += 1
    request_id = client.sequence
    client.send({'jsonrpc':'2.0','id':request_id,'method':'tools/call',
                 'params':{'name':'vm_guest_info','arguments':{'id':vm}}})
    deadline = time.monotonic() + timeout
    while b'\n' not in client.buffer:
        remaining = deadline-time.monotonic()
        require(remaining > 0 and select.select([client.p.stdout],[],[],remaining)[0],
                'diagnostic Guest info timeout')
        data = os.read(client.p.stdout.fileno(),65536)
        require(data, 'diagnostic Guest info EOF')
        client.buffer.extend(data)
        require(len(client.buffer) <= 262144, 'diagnostic Guest info bound')
    line,_,rest = client.buffer.partition(b'\n'); client.buffer=bytearray(rest)
    require(len(line) <= 262144, 'diagnostic Guest info line bound')
    value=json.loads(line)
    require(value.get('id') == request_id, 'diagnostic Guest info wrong response ID')
    return value  # Error responses are evidence, never interpreted as healthy.


def capture_before_cleanup(vms, observation, status, chain, journal, info, save):
    """Save all peers independently BEFORE the caller can mutate either VM."""
    observation.update(phase='before_any_force_stop', vms={})
    for label,v in vms.items():
        if not v['started']: continue
        row={'id':v['id'],'original_pins':v['pins']}
        observation['vms'][label]=row; save()
        for name,read in (('status',lambda:status(v)), ('processes',lambda:chain(v)),
                          ('journal',journal)):
            try: row[name]=read()
            except Exception as error: row[name+'_error_type']=type(error).__name__
            save()
    # First pin both VMs' pre-query evidence. Queries cannot fabricate a signal
    # proof and do not authorize commands, acquire a lease or arm the watcher.
    for label,v in vms.items():
        if not v['started']: continue
        row=observation['vms'][label]; began=time.monotonic()
        try: row['guest_info_reply']=info(v)
        except Exception as error: row['guest_info_error_type']=type(error).__name__
        row['guest_info_elapsed_seconds']=time.monotonic()-began; save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'worker', 'state', 'a-config', 'a-base', 'b-fixture', 'b-maintenance', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--journal-version',type=int,choices=(2,3),default=2,
                        help='strict selected schema; v3 also requires completed witness GC, never GC of pending B')
    args = parser.parse_args()
    for key, value in vars(args).items():
        if isinstance(value,Path): setattr(args, key, value.resolve())
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    out = args.output / 'result.json'
    with os.fdopen(os.open(out, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600), 'w') as f: f.write('{}\n')
    report = {'passed': False, 'scope': 'same-daemon installed Linux peer isolation after Guest service SIGKILL',
              'journal_version': args.journal_version, 'checks': [], 'fault_timeline': [], 'shutdown_timeline': [],
              'limits': ['B scope cleanup stays unconfirmed; no synthetic Guest cgroup-empty ACK',
                         'No daemon death, three-OS, performance or continuous screenshot claim',
                         'A long-job continuity must be observed before its 30s timeout',
                         'No reconciliation and no removal of B pending journal']}
    clients, vms = [], {}
    daemon = None
    private_nonce = None

    def save():
        text = json.dumps(report, indent=2) + '\n'
        if private_nonce: text = text.replace(private_nonce, '<one-shot-nonce-redacted>')
        out.write_text(text)
    def record(name, value):
        report['checks'].append({'name': name, 'evidence': value}); save(); print(name, flush=True)
    def call(method, data):
        require(method in ('list', 'create', 'status', 'shutdown', 'force_stop'), 'harness local method forbidden')
        if method != 'list': require(data.get('id') in [v['id'] for v in vms.values()], 'unowned VM operation')
        result = subprocess.run([str(args.binary), 'call', '--state-dir', str(args.state), method, json.dumps(data)],
                                capture_output=True, text=True, timeout=42)
        require(result.returncode == 0 and len(result.stdout) <= 2*1024**2, 'local operation failed: ' + method)
        return json.loads(result.stdout)
    def tool(v, method, **data): return v['owner'].tool(method, {'id': v['id'], **data})
    def execute(v, argv, timeout=2000):
        job = tool(v, 'exec_start', argv=argv, timeout_ms=timeout)['result']['job_id']
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            value = tool(v, 'exec_poll', job_id=job)['result']
            if value['state'] == 'finished':
                require('error' not in value, 'actual command containment failure')
                done = value['result']
                require(done['exit_code'] == 0 and all(done[k] is False for k in
                    ('cancelled', 'timed_out', 'stdout_truncated', 'stderr_truncated')), 'actual rootless oracle failed')
                return done['stdout']
            time.sleep(.03)
        raise AssertionError('actual command deadline')
    def observe(v):
        return oracle.parse_observation(execute(v, ['/bin/sh', '-eu', '-c', oracle.READ, 'observe', v['path']]))
    def screenshot(v, label):
        reply = v['owner'].tool('screenshot', {'id': v['id']}, raw=True)
        images = [x for x in reply['result']['content'] if x.get('type') == 'image']
        require(len(images) == 1 and images[0]['mimeType'] == 'image/png', 'actual PNG missing')
        pixels = base64.b64decode(images[0]['data'], validate=True)
        require(0 < len(pixels) <= 8*1024**2, 'PNG bound')
        path = args.output / (label + '.png')
        with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), 'wb') as f: f.write(pixels)
        decoded = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_entries',
            'stream=codec_name,width,height', '-of', 'json', str(path)], timeout=15))
        require(decoded['streams'] == [{'codec_name': 'png', 'width': v['config']['width'],
                                       'height': v['config']['height']}], 'actual image dimensions mismatch')
        return {'sha256': digest(path), 'bytes': len(pixels), 'decode': decoded}
    def journal():
        # Atomic publication can replace the pinned file between fstat/lstat.
        # Retry ONLY this observed race, never schema/security/IO failures.
        for attempt in range(5):
            try: return witness.read_journal(args.state,args.journal_version)[0]
            except AssertionError as error:
                if str(error) != 'journal changed while reading': raise
                report.setdefault('journal_publication_retries', []).append({'attempt':attempt+1})
                save(); time.sleep(.01)
        raise AssertionError('journal publication did not stabilize')
    def chain(v): return [process.snapshot(p['pid']) for p in v['pins']]
    def peer_sample(a, before):
        status = call('status', {'id': a['id']}); pins = chain(a)
        require(status['state'] == 'running' and status['pid'] == a['pins'][0]['pid'], 'A original worker not running')
        same_live_chain(a['pins'], pins)
        require(status['runtime']['execution_witness'] == a['runtime_witness'], 'A witness changed')
        preserve_peer(before, journal(), a['identity'],args.journal_version)
        require(process.same_incarnation(daemon, process.snapshot(daemon['pid'])), 'shared daemon changed')
        poll = tool(a, 'exec_poll', job_id=a['job'])['result']; require(poll['state'] == 'running', 'A long job stopped before fault observation')
        observation = observe(a)
        return {'status': status, 'processes': pins, 'job_state': poll['state'], 'growth': observation}

    try:
        require(call('list', {}) == [], 'empty dedicated daemon required (not control-macos)')
        initial = journal()
        require(initial == {'version': args.journal_version, 'scopes': [], 'launches': [], 'completed': []}, 'fresh selected-version journal required')
        prep = json.loads((args.b_fixture / 'preparation.json').read_text())
        bcfgpath = args.b_fixture / 'installed.json'
        require(prep['prepared'] is True and digest(bcfgpath) == prep['config_sha256']['installed.json'], 'B prepared config mismatch')
        maintenance = json.loads(args.b_maintenance.read_text())
        require(maintenance.get('passed') is True and maintenance.get('disk_binding_verified') is True
                and maintenance['status']['state'] == 'stopped' and maintenance['status'].get('exit_code') == 0,
                'B actual maintenance PASS required')
        configs = {'A': json.loads(args.a_config.read_text()), 'B': json.loads(bcfgpath.read_text())}
        require(configs['A']['id'] != configs['B']['id'], 'distinct VM IDs required')
        nonce = re.findall(r'(?:^| )veynode.guest_crash_once=([0-9a-f]{64})(?: |$)', configs['B']['command_line'])
        require(len(nonce) == 1 and 'veynode.guest_crash_once=' not in configs['A'].get('command_line', ''), 'one-shot belongs only to B')
        nonce = nonce[0]; private_nonce = nonce
        disk_ids, base_ids = [], []
        for label, cfg in configs.items():
            require(cfg.get('qemu_guest_exec') is True and cfg.get('backend') == 'qemu-hvf'
                    and cfg.get('guest') == 'linux' and cfg['cpus'] <= 2 and cfg['memory_mib'] <= 1536,
                    'bounded installed Linux execution fixture required')
            disk = Path(cfg['disk']).resolve(); immutable = args.a_base if label == 'A' else args.b_fixture/'images/bases/exec-base/disk.raw'
            ds, bs = disk.stat(), immutable.stat()
            require(disk.is_relative_to(ROOT/'.local') and immutable.is_relative_to(ROOT/'.local')
                    and stat.S_ISREG(ds.st_mode) and stat.S_ISREG(bs.st_mode)
                    and stat.S_IMODE(ds.st_mode) == 0o600 and not bs.st_mode & 0o222
                    and (ds.st_dev, ds.st_ino) != (bs.st_dev, bs.st_ino), 'independent private clone/read-only base required')
            disk_ids.append((ds.st_dev, ds.st_ino)); base_ids.append((bs.st_dev, bs.st_ino))
            if label == 'B':
                require(disk == args.b_fixture/'images/clones/guest-crash/disk.raw', 'B exact clone required')
                require(Path(maintenance['status']['config']['disk']).resolve() == disk, 'maintenance covered a different disk')
                require(digest(immutable) == prep['imported_sha256'], 'B immutable base mismatch')
            cfg.update(window=False, serial_log=str(args.output/(label + '.serial.log')))
            vms[label] = {'id': cfg['id'], 'config': cfg, 'disk': ds, 'base': immutable, 'base_sha256': digest(immutable),
                          'started': False, 'pins': [], 'path': '/home/veynode/peer-'+secrets.token_hex(8)}
        require(len(set(disk_ids)) == 2 and not set(disk_ids).intersection(base_ids),
                'A/B hardlinked, same writable inode or a peer base')
        record('frozen artifacts and independent disks', {'controller_sha256': digest(args.binary), 'worker_sha256': digest(args.worker),
            'a_config_sha256': digest(args.a_config), 'b_config_sha256': digest(bcfgpath), 'b_maintenance_sha256': digest(args.b_maintenance),
            'watcher_sha256': prep['watcher_sha256'], 'script_sha256': digest(Path(__file__)),
            'helper_sha256': {f: digest(ROOT/'scripts'/f) for f in ('test-installed-qemu-exec.py',
                'test-installed-qemu-desktop.py', 'test-qemu-exec-guardian.py', 'test-qemu-exec-witness-recovery.py',
                'test-qemu-guest-crash.py', 'test-installed-qemu-exec-lost-ack.py')},
            'disks': {k: {'device': v['disk'].st_dev, 'inode': v['disk'].st_ino, 'base_sha256': v['base_sha256']} for k,v in vms.items()}})
        # B first; no continuity job exists while either VM is still booting.
        for label in ('B', 'A'):
            v = vms[label]; call('create', v['config'])
            v['life'] = base.MCP(args.binary, args.state, False); clients.append(v['life'])
            v['owner'] = base.MCP(args.binary, args.state, True); clients.append(v['owner'])
            v['started'] = True
            v['life'].tool('start', {'id': v['id']})
            deadline, boot = time.monotonic()+180, None
            log = Path(v['config']['serial_log'])
            while time.monotonic() < deadline:
                status = call('status', {'id': v['id']})
                require(status['state'] in ('starting','running'), 'Guest stopped before installed readiness')
                if log.exists():
                    require(log.stat().st_size <= base.desktop.MAX_LOG, 'serial bound')
                    boot = base.desktop.boot_evidence(log.read_bytes())
                    if boot: break
                time.sleep(.2)
            require(boot is not None, 'actual installed graphics readiness timeout')
            runtime = status['runtime']; g = runtime['execution_guardian']; dp = runtime['disk_provenance']
            require(dp.get('device') == v['disk'].st_dev and dp.get('inode') == v['disk'].st_ino
                    and dp.get('descriptor_backed') is True and dp.get('qmp_fdsets_verified_at_start') is True, 'disk binding mismatch')
            require(g.get('owns_qemu_child') is True and g['backend_pid'] == runtime['backend_pid'], 'guardian missing')
            v['pins'] = [process.snapshot(p) for p in (status['pid'],g['guardian_pid'],g['backend_pid'])]
            require(all(v['pins']) and all(p['uid'] == os.getuid() for p in v['pins'])
                    and v['pins'][0]['executable'] == str(args.worker) and v['pins'][1]['executable'] == str(args.worker)
                    and v['pins'][1]['ppid'] == v['pins'][0]['pid'] and v['pins'][2]['ppid'] == v['pins'][1]['pid'], 'process chain mismatch')
            d = process.snapshot(v['pins'][0]['ppid'])
            require(d and d['executable'] == str(args.binary) and d['uid'] == os.getuid()
                    and Path(process.option(process.command_arguments(d['pid']), '--state-dir')).resolve() == args.state, 'dedicated daemon identity mismatch')
            if daemon: require(process.same_incarnation(daemon, d), 'VMs have different daemon parents')
            daemon = d
            instance = process.option(process.command_arguments(v['pins'][0]['pid']), '--guest-worker-instance')
            v['identity'] = witness.witness_from_runtime(runtime, v['id'], instance, args.state)
            v['runtime_witness'] = runtime['execution_witness']; v['boot'] = boot
            info, attempts = base.wait_guest_info(v['owner'], v['id'], lambda: call('status', {'id':v['id']}), status['pid'])
            require(info.get('transport') == 'virtio-serial-private-exec-v2' and info.get('transport_reconnect') is False
                    and info.get('execution_containment') == 'cgroup-v2-clone3-kill'
                    and info.get('execution_identity',{}).get('rootless') is True, 'real rootless containment unavailable')
            record(label+' installed ready', {'boot':boot, 'info':info, 'temporary_refusals':attempts,
                   'pins':v['pins'], 'daemon':daemon, 'identity':v['identity']})
            tool(v, 'acquire_control')
        a,b = vms['A'],vms['B']
        ready = crash.events(Path(b['config']['serial_log']))
        require(len(ready) == 1 and ready[0]['event'] == 'watcher_ready', 'B one-shot already consumed/refused')
        for v in (b,a):
            v['job'] = tool(v,'exec_start',argv=['/bin/sh','-c',oracle.OUTER,'peer',v['path'],oracle.CHILD],timeout_ms=30000)['result']['job_id']
            require(re.fullmatch('[0-9a-f]{32}',v['job']), 'actual long job ACK required')
        time.sleep(.6)
        a0,b0 = observe(a),observe(b); time.sleep(.3); a1,b1 = observe(a),observe(b)
        increasing(a0,a1); increasing(b0,b1)
        initial_active = journal()
        ascope, bscope = active_projection(initial_active,a['identity'],args.journal_version), active_projection(initial_active,b['identity'],args.journal_version)
        record('both rootless detached jobs making progress', {'A':[a0,a1],'B':[b0,b1],'journal':initial_active})
        record('A real screenshot before peer fault', screenshot(a,'A-before'))
        begin = time.monotonic()
        arm = b['owner'].rpc('tools/call',{'name':'vm_exec_start','arguments':{'id':b['id'],'argv':['/bin/sh','-eu','-c',
            'printf "%s\\n" "$1" > /var/lib/veynode-crash-once/arm','arm',nonce],'timeout_ms':2000}})
        record('B rootless one-shot arm (ACK not kill proof)', {'rpc_error':'error' in arm or arm.get('result',{}).get('isError') is True})
        absence, next_a = 0,0
        while time.monotonic()-begin < 15:
            status = call('status',{'id':b['id']}); observed = chain(b)
            absence = absence+1 if all(not process.same_incarnation(p,q) for p,q in zip(b['pins'],observed)) else 0
            row = {'elapsed_seconds':time.monotonic()-begin,'B_status':status,'B_processes':observed,'B_absent_samples':absence}
            report['fault_timeline'].append(row); save()
            if time.monotonic() >= next_a:
                row['A'] = peer_sample(a,ascope); save(); next_a = time.monotonic()+.4
            if absence >= 2 and status['state'] in ('crashed','error','stopped','recovery_required'): break
            time.sleep(.05)
        require(absence >= 2 and time.monotonic()-begin < 15, 'B original incarnation retirement deadline')
        signal_proof = crash.crash_proof(crash.events(Path(b['config']['serial_log'])),prep['preserved_guest_sha256']['usr/local/bin/veynode-guest'])
        b['started'] = False
        pending_witness_intact(args.state,b['identity'])
        final_a = peer_sample(a,ascope); increasing(a1,final_a['growth'])
        preserve_peer(bscope,journal(),b['identity'],args.journal_version)
        record('B real SIGKILL and retirement; same A job still grows', {'signal':signal_proof,'B_retirement_seconds':time.monotonic()-begin,'A':final_a,'guest_empty_ack':False})
        denied = b['owner'].tool('exec_start',{'id':b['id'],'argv':['/bin/true']},denied=True)
        require('error' in denied or denied.get('result',{}).get('isError') is True,'B new exec not denied')
        try: b['owner'].close()
        except AssertionError: pass
        require(b['owner'].p.returncode == 1,'B original MCP EOF must report unconfirmed cleanup')
        clients.remove(b['owner']); preserve_peer(bscope,journal(),b['identity'],args.journal_version)
        fresh = execute(a,['/bin/sh','-eu','-c','test "$(id -u)" = 1000; cat /proc/sys/kernel/random/boot_id'])
        require(fresh.strip() == a['boot']['boot']['boot_id'],'A new rootless command or same boot identity failed')
        record('A new execution and PNG survive B unknown-scope EOF', {'boot_id':fresh.strip(),'screenshot':screenshot(a,'A-after'),'B_MCP_exit':1})
        # Cancel the same long job explicitly; a timeout/completion is not cancellation proof.
        require(tool(a,'exec_cancel',job_id=a['job'])['result']['cancel_requested'] is True,'A cancel request failed')
        deadline = time.monotonic()+5; terminal = None
        while time.monotonic()<deadline:
            value = tool(a,'exec_poll',job_id=a['job'])['result']
            if value['state']=='finished': terminal=value; break
            time.sleep(.03)
        require(terminal is not None and 'error' not in terminal and terminal['result']['cancelled'] is True
                and terminal['result']['timed_out'] is False,'A original job cleanup not proved')
        release_acknowledged(tool(a,'release_control'))
        a['owner'].close(); clients.remove(a['owner'])
        preserve_peer(bscope,journal(),b['identity'],args.journal_version)
        record('A original scope cleaned independently', {'terminal':terminal,'MCP_exit':0,'journal':journal()})
        before_normal=journal()
        request = call('shutdown',{'id':a['id']})
        require(request.get('requested') is True and request.get('mechanism')=='qmp-system-powerdown', 'A real ACPI shutdown required')
        record('A normal shutdown requested',request)
        deadline, absent = time.monotonic()+65,0
        while time.monotonic()<deadline:
            status = call('status',{'id':a['id']})
            row={'status':status,'elapsed_seconds':65-max(0,deadline-time.monotonic())}
            report['shutdown_timeline'].append(row); save()
            snap=journal(); row['journal']=snap; save()
            observations=chain(a)
            absent=absent+1 if all(not process.same_incarnation(p,q) for p,q in zip(a['pins'],observations)) else 0
            row.update(processes=observations,absent_samples=absent); save()
            preserve_peer(bscope,snap,b['identity'],args.journal_version)
            # Pending B metadata must survive even after its backend is gone.
            pending_witness_intact(args.state,b['identity'])
            if normal_proof_ready(status,snap,a['identity'],args.journal_version) and absent>=2:
                witness.verify_transition(before_normal,snap,a['identity'],args.journal_version)
                if args.journal_version == 3:
                    row['A_completed_witness_gc']=completed_gc_ready(args.state,a['identity']);save()
                    if not row['A_completed_witness_gc']: time.sleep(.05);continue
                break
            time.sleep(.15)
        require(normal_proof_ready(status,snap,a['identity'],args.journal_version) and absent>=2,'A normal exit/exact durable proof deadline')
        require(b'VEYNODE_INSTALLED_ACPI_POWER_BUTTON' in Path(a['config']['serial_log']).read_bytes(),'actual A ACPI handler missing')
        if args.journal_version == 3:
            witness.require_completed_gc(args.state,a['identity'])
        pending_witness_intact(args.state,b['identity'])
        record('selected-version completion and pending witness retention',
               {'journal_version':args.journal_version,'A_completed_witness_absent':True if args.journal_version==3 else None,
                'A_gc_asserted':args.journal_version==3,'B_pending_witness_identity_verified':b['identity']})
        a['started']=False
        for v in (a,b): v['life'].close(); clients.remove(v['life'])
        require(process.same_incarnation(daemon,process.snapshot(daemon['pid'])),'shared daemon died')
        for v in (a,b): require(digest(v['base'])==v['base_sha256'],'immutable base changed')
        record('normal A exit and B-only unknown journal retained', {'journal':journal(),'A_status':status,'daemon':daemon,
               'bases_unchanged':True,'B_scope_cleanup_confirmed':False})
        report['passed']=True
    except BaseException as error:
        report['error']=re.sub(r'[0-9a-f]{32,64}','<redacted>',str(error)); raise
    finally:
        # Never signal a daemon, call reconcile, or touch a VM ID outside this test.
        if any(v['started'] for v in vms.values()):
            report['pre_cleanup_observation']={}
            capture_before_cleanup(vms,report['pre_cleanup_observation'],
                lambda v:call('status',{'id':v['id']}),chain,journal,
                lambda v:bounded_guest_info(v['life'],v['id']),save)
        for label,v in vms.items():
            if v['started']:
                item={'vm':v['id'],'observations':[]}; report.setdefault('failure_cleanup',{})[label]=item
                try:
                    call('force_stop',{'id':v['id'],'confirm':v['id']}); deadline=time.monotonic()+15; absent=0
                    while time.monotonic()<deadline:
                        status=call('status',{'id':v['id']}); now=chain(v)
                        absent=absent+1 if v['pins'] and all(not process.same_incarnation(p,q) for p,q in zip(v['pins'],now)) else 0
                        item['observations'].append({'status':status,'processes':now,'absent_samples':absent}); save()
                        if absent>=2 and status['state'] in ('stopped','crashed','error','recovery_required'):break
                        time.sleep(.1)
                    item['process_retirement_confirmed']=absent>=2
                    item['scope_cleanup_confirmed']=False
                except Exception as error:item['error_type']=type(error).__name__
        for client in clients:
            try:client.close()
            except Exception:pass
            report.setdefault('final_client_exits',[]).append(client.p.returncode)
        save()


if __name__=='__main__':main()
