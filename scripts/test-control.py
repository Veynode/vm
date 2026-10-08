#!/usr/bin/env python3
"""Real Apple VM → daemon → actual MCP stdio client smoke; no fake VM backend.
Usage: python3 scripts/test-control.py --config .local/run/linux-smoke.json [--require-guest]
VMs created by this test are explicitly force-stopped on cleanup. Input disk must be a disposable test disk.
"""
import argparse
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--config', type=Path, required=True)
parser.add_argument('--require-guest', action='store_true')
parser.add_argument('--jobs', action='store_true')
parser.add_argument('--output', type=Path, default=ROOT / '.local/evidence/control-smoke.json')
args = parser.parse_args()
if args.jobs: args.require_guest=True
binary = ROOT / 'target/debug/vm-control'
backend = ROOT / '.local/bin/vm-apple'
config = json.loads(args.config.read_text())
config['id'] = 'control-smoke'
# Unique serial output keeps this check independent of other concurrently running fixtures.
args.output.parent.mkdir(parents=True, exist_ok=True)
config['serial_log'] = str(args.output.parent.resolve() / 'control-smoke-serial.log')
result = {'backend': str(backend), 'config': config, 'started_at_unix': time.time(), 'checks': []}

def record(name, evidence):
    result['checks'].append({'name': name, 'evidence': evidence})

with tempfile.TemporaryDirectory(prefix='vm-control-') as tmp:
    state = Path(tmp) / 'state'
    daemon = subprocess.Popen([str(binary), 'daemon', '--state-dir', str(state), '--backend', str(backend)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    clients = []
    def call(method, payload=None):
        output = subprocess.check_output([str(binary), 'call', '--state-dir', str(state), method, json.dumps(payload or {})], timeout=45)
        return json.loads(output)
    def client(lifecycle=False, execute=False):
        command = [str(binary), 'mcp', '--state-dir', str(state)]
        if lifecycle: command.append('--allow-lifecycle')
        if execute: command.append('--allow-execute')
        p = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        clients.append(p)
        return p
    def send(p, request):
        p.stdin.write(json.dumps(request) + '\n'); p.stdin.flush()
        if 'id' not in request: return
        if not select.select([p.stdout], [], [], 45)[0]: raise TimeoutError('MCP response timeout')
        line = p.stdout.readline()
        if not line: raise RuntimeError('MCP unexpectedly closed stdout')
        return json.loads(line)
    def initialize(p):
        reply = send(p, {'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'veynode-real-validation','version':'1'}}})
        assert reply['result']['protocolVersion'] == '2025-06-18', reply
        send(p, {'jsonrpc':'2.0','method':'notifications/initialized'})
    def tool(p, name, arguments, number):
        reply = send(p, {'jsonrpc':'2.0','id':number,'method':'tools/call','params':{'name':name,'arguments':arguments}})
        assert not reply.get('error'), reply
        assert not reply['result'].get('isError'), reply
        return json.loads(reply['result']['content'][0]['text'])
    try:
        for _ in range(200):
            if state.joinpath('control.sock').exists(): break
            if daemon.poll() is not None: raise RuntimeError(daemon.stderr.read().decode())
            time.sleep(.025)
        p = client(lifecycle=True, execute=args.require_guest); initialize(p)
        tool(p, 'vm_create', config, 2)
        record('MCP start accepted', tool(p, 'vm_start', {'id':config['id']}, 3))
        for _ in range(240):
            status = call('status', {'id': config['id']})
            if status['state'] == 'running': break
            if status['state'] in ('crashed', 'error', 'stopped'): raise RuntimeError(status)
            time.sleep(.25)
        else: raise TimeoutError('Native backend did not enter running')
        record('real native backend running', status)
        record('close console', tool(p, 'vm_close_window', {'id':config['id']}, 4))
        p.stdin.close(); assert p.wait(timeout=5) == 0
        record('MCP disconnect preserves VM', call('status', {'id':config['id']}))
        assert result['checks'][-1]['evidence']['state'] == 'running'
        observer = client(); initialize(observer)
        denied = send(observer, {'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'vm_exec','arguments':{'id':config['id'],'argv':['/bin/true']}}})
        assert denied['error']['code'] == -32602, denied
        record('observe session rejects execution', denied)
        if args.require_guest:
            executor = client(execute=True); initialize(executor)
            for n in range(60):
                try:
                    info = tool(executor, 'vm_guest_info', {'id':config['id']}, 10+n)
                    break
                except AssertionError:
                    time.sleep(.25)
            else: raise TimeoutError('Guest service unavailable')
            record('actual guest info after console closed', info)
            record('control acquired', tool(executor, 'vm_acquire_control', {'id':config['id']}, 79))
            execution = tool(executor, 'vm_exec', {'id':config['id'],'argv':['/bin/uname','-m'],'timeout_ms':5000}, 80)
            assert execution['result']['exit_code'] == 0 and 'aarch64' in execution['result']['stdout'], execution
            record('actual guest command via MCP', execution)
            record('local human takeover', call('human_takeover', {'id':config['id']}))
            revoked = send(executor, {'jsonrpc':'2.0','id':81,'method':'tools/call','params':{'name':'vm_exec','arguments':{'id':config['id'],'argv':['/bin/true']}}})
            assert revoked['result']['isError'], revoked
            record('stale lease denied after human takeover', revoked)
            call('human_release', {'id':config['id']})
            record('reacquired after human release', tool(executor, 'vm_acquire_control', {'id':config['id']}, 82))
            if args.jobs:
                def job_start(argv, number):
                    response=tool(executor,'vm_exec_start',{'id':config['id'],'argv':argv,'timeout_ms':30000},number)
                    return response['result']['job_id']
                def job_finished(job_id,number):
                    for n in range(100):
                        reply=tool(executor,'vm_exec_poll',{'id':config['id'],'job_id':job_id},number+n)['result']
                        if reply['state']=='finished':return reply
                        time.sleep(.05)
                    raise TimeoutError('guest job did not terminate')
                job_id=job_start(['/bin/sleep','20'],100)
                outsider=client(execute=True);initialize(outsider)
                for operation in ['vm_exec_poll','vm_exec_cancel']:
                    reply=send(outsider,{'jsonrpc':'2.0','id':101,'method':'tools/call','params':{'name':operation,'arguments':{'id':config['id'],'job_id':job_id}}})
                    assert reply['result']['isError'],reply
                    record('other MCP connection denied '+operation,reply)
                cancellation=tool(executor,'vm_exec_cancel',{'id':config['id'],'job_id':job_id},102)
                assert cancellation['result']['cancel_requested'],cancellation
                finished=job_finished(job_id,110);assert finished['result']['cancelled'] and not finished['result']['timed_out'],finished
                record('actual async cancellation confirmed by terminal poll',finished)
                job_id=job_start(['/bin/sleep','20'],220)
                takeover=call('human_takeover',{'id':config['id']})
                assert takeover['job_cancellation']['attempted']==1,takeover
                assert takeover['job_cancellation']['unconfirmed_requests']==0,takeover
                finished=job_finished(job_id,230);assert finished['result']['cancelled'],finished
                record('human takeover cancelled actual guest job',{'takeover':takeover,'terminal':finished})
                revoked=send(executor,{'jsonrpc':'2.0','id':340,'method':'tools/call','params':{'name':'vm_exec_start','arguments':{'id':config['id'],'argv':['/bin/true']}}})
                assert revoked['result']['isError'],revoked
                call('human_release',{'id':config['id']});tool(executor,'vm_acquire_control',{'id':config['id']},341)
                original=tool(executor,'vm_exec_poll',{'id':config['id'],'job_id':job_id},342)
                assert original['result']['state']=='finished';record('original epoch ownership survives new lease acquisition',True)
                job_id=job_start(['/bin/printf','你好 ✓'],350);finished=job_finished(job_id,360)
                assert finished['result']['stdout']=='你好 ✓' and finished['result']['exit_code']==0 and not finished['result']['cancelled'],finished
                record('actual async Unicode command completion',finished)
                tool(executor,'vm_release_control',{'id':config['id']},470)
                for case in ['normal-eof','unterminated-frame','broken-stdout']:
                    closer=client(execute=True);initialize(closer)
                    tool(closer,'vm_acquire_control',{'id':config['id']},500)
                    ready='/tmp/veynode-'+case+'-ready';late='/tmp/veynode-'+case+'-late'
                    started=tool(closer,'vm_exec_start',{'id':config['id'],'argv':['/bin/sh','-c',f'printf ready > {ready}; sleep 2; printf late > {late}'],'timeout_ms':10000},501)
                    for n in range(50):
                        probe=tool(closer,'vm_exec',{'id':config['id'],'argv':['/bin/sh','-c',f'test -f {ready}']},502+n)
                        if probe['result']['exit_code']==0:break
                        time.sleep(.02)
                    else:raise AssertionError('marker job never started')
                    if case=='unterminated-frame':closer.stdin.write('{"jsonrpc":');closer.stdin.flush()
                    if case=='broken-stdout':
                        closer.stdout.close();closer.stdin.write(json.dumps({'jsonrpc':'2.0','id':559,'method':'ping'})+'\n');closer.stdin.flush()
                    else:closer.stdin.close()
                    exit_code=closer.wait(timeout=15)
                    if not closer.stdin.closed:closer.stdin.close()
                    assert exit_code==(0 if case=='normal-eof' else 1),exit_code
                    time.sleep(2.2)
                    tool(outsider,'vm_acquire_control',{'id':config['id']},560)
                    absent=tool(outsider,'vm_exec',{'id':config['id'],'argv':['/bin/sh','-c',f'test ! -e {late}']},561)
                    assert absent['result']['exit_code']==0,absent
                    record('MCP '+case+' cancels running job and releases lease',{'job_id':started['result']['job_id'],'late_marker_absent':True})
                    tool(outsider,'vm_release_control',{'id':config['id']},562)


        assert call('status', {'id':config['id']})['state'] == 'running'
        result['passed'] = True
    except Exception as exc:
        result['passed'] = False; result['error'] = str(exc)
        raise
    finally:
        for p in clients:
            if p.poll() is None:
                if not p.stdin.closed: p.stdin.close()
                try: p.wait(timeout=5)
                except subprocess.TimeoutExpired: p.kill(); p.wait()
        try: record('cleanup force stop', call('force_stop', {'id':config['id'],'confirm':config['id']}))
        except Exception as exc: result['cleanup_error'] = str(exc)
        daemon.terminate()
        try: daemon.wait(timeout=5)
        except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
        result['finished_at_unix'] = time.time()
        output=args.output if result.get('passed') else args.output.with_suffix('.failed.json')
        output.write_text(json.dumps(result, indent=2) + '\n')
print(f"Real control smoke passed; evidence: {args.output}")
