#!/usr/bin/env python3
"""Actual HTTPS + stdio MCP subprocess integration. Uses a temporary trusted test CA, never insecure TLS.
Optional --config points to a disposable Linux fixture to verify actual guest commands across HTTPS.
"""
import argparse
import base64
import concurrent.futures
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import socket
import ssl
import subprocess
import tempfile
import time

ROOT=Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--config',type=Path);p.add_argument('--sdk',action='store_true');p.add_argument('--files',action='store_true');p.add_argument('--node-sdk',action='store_true');p.add_argument('--output',type=Path,default=ROOT/'.local/evidence/control-http.json');args=p.parse_args()
result={'checks':[],'started_at_unix':time.time(),'tls_verification':'explicit trusted test certificate; hostname verification enabled','topology':'localhost HTTPS terminates on Mac; no cloud relay'}
def record(name,evidence=True):result['checks'].append({'name':name,'evidence':evidence})
with tempfile.TemporaryDirectory(prefix='vm-https-') as tmp:
    tmp=Path(tmp);state=tmp/'state';cert=tmp/'cert.pem';key=tmp/'key.pem';config=tmp/'https.json'
    subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-sha256','-keyout',str(key),'-out',str(cert),'-days','1','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost,IP:127.0.0.1'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    key.chmod(0o600)
    tokens={name:secrets.token_urlsafe(32) for name in ['observer','operator','inputter',*(['filer'] if args.files else [])]}
    with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
    config.write_text(json.dumps({'bind':f'127.0.0.1:{port}','tls_cert':str(cert),'tls_key':str(key),'allowed_origins':['https://client.example'],'session_idle_seconds':5,'principals':[{'name':name,'token_sha256':hashlib.sha256(token.encode()).hexdigest(),'vm_ids':['https-smoke'],'scopes':['observe'] if name=='observer' else (['observe','files'] if name=='filer' else (['observe','input'] if name=='inputter' else ['observe','lifecycle','execute']))} for name,token in tokens.items()]}));config.chmod(0o600)
    daemon=subprocess.Popen([str(ROOT/'target/debug/vm-control'),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    for _ in range(100):
        if state.joinpath('control.sock').exists():break
        time.sleep(.02)
    server=subprocess.Popen([str(ROOT/'target/debug/vm-mcp-http'),'--config',str(config),'--state-dir',str(state)],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    context=ssl.create_default_context(cafile=str(cert))
    def request(method='POST',data=None,principal='observer',session=None,extra=None):
        headers={'Accept':'application/json, text/event-stream','Content-Type':'application/json','MCP-Protocol-Version':'2025-06-18'}
        if principal:headers['Authorization']='Bearer '+tokens.get(principal,principal)
        if session:headers['MCP-Session-Id']=session
        if extra:headers.update(extra)
        connection=http.client.HTTPSConnection('127.0.0.1',port,context=context,timeout=45)
        body=json.dumps(data).encode() if data is not None else b''
        connection.request(method,'/mcp',body,headers)
        response=connection.getresponse();payload=response.read();status=response.status;reply_headers=dict(response.getheaders());connection.close()
        try: decoded=json.loads(payload) if payload else None
        except json.JSONDecodeError: decoded=payload.decode(errors='replace')
        return status,decoded,reply_headers
    def rpc(method,number=1,params=None):
        return {'jsonrpc':'2.0','id':number,'method':method,**({'params':params} if params is not None else {})}
    def initialize(principal):
        status,reply,headers=request(data=rpc('initialize',params={'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'real-https-validator','version':'1'}}),principal=principal)
        assert status==200 and reply['result']['protocolVersion']=='2025-06-18',(status,reply)
        sid=headers['mcp-session-id'];assert request(data={'jsonrpc':'2.0','method':'notifications/initialized'},principal=principal,session=sid)[0]==202
        return sid
    def tool(session,name,arguments,number=4,principal='operator'):
        status,reply,_=request(data=rpc('tools/call',number,{'name':name,'arguments':arguments}),principal=principal,session=session)
        assert status==200 and not reply.get('error') and not reply['result'].get('isError'),(status,reply)
        return json.loads(reply['result']['content'][0]['text'])
    vm_created=False
    try:
        for _ in range(200):
            try:
                if request('GET',principal=None)[0]==401:break
            except OSError:pass
            if server.poll() is not None:raise RuntimeError(server.stderr.read().decode())
            time.sleep(.025)
        else:raise TimeoutError('HTTPS server did not listen')
        # A client without this CA must reject the certificate, proving verification is not bypassed.
        try:
            untrusted=http.client.HTTPSConnection('127.0.0.1',port,timeout=3);untrusted.request('GET','/mcp');untrusted.getresponse()
            raise AssertionError('Untrusted TLS certificate unexpectedly accepted')
        except ssl.SSLCertVerificationError:record('untrusted certificate rejected')
        assert request(data=rpc('ping'),principal=None)[0]==401;record('missing bearer denied')
        assert request(data={'jsonrpc':'2.0','method':'ping','params':{'padding':'x'*(1024*1024)}})[0]==413;record('oversized request rejected')
        assert request(data=[rpc('ping')])[0]==400;record('batch envelope rejected')
        assert request(data=rpc('ping'),principal='x'*32)[0]==401;record('wrong bearer denied')
        assert request(data=rpc('ping'),extra={'Origin':'https://evil.example'})[0]==403;record('untrusted browser Origin denied')
        assert request('OPTIONS',principal=None,extra={'Origin':'https://client.example'})[0]==204;record('allowed browser preflight')
        assert request(data=rpc('ping'),extra={'Content-Type':'text/plain'})[0]==415;record('content type enforced')
        assert request(data=rpc('ping'),extra={'Accept':'application/json'})[0]==406;record('streamable HTTP Accept enforced')
        assert request(data=rpc('ping'),extra={'MCP-Protocol-Version':'2099-01-01'})[0]==400;record('protocol version enforced')
        observer=initialize('observer');operator=initialize('operator')
        extras=[initialize('observer') for _ in range(3)]
        quota=request(data=rpc('initialize',params={'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'quota-test','version':'1'}}))[0]
        assert quota==429;record('principal session quota enforced')
        for sid in extras: assert request('DELETE',session=sid)[0]==204
        assert request(data=rpc('tools/list'),principal='operator',session=observer)[0]==404;record('session bound to authenticated principal')
        status,reply,_=request(data=rpc('tools/list'),session=observer);assert status==200
        names=[t['name'] for t in reply['result']['tools']];assert 'vm_exec' not in names and 'vm_start' not in names;record('scope-filtered tools',names)
        assert request('GET',session=observer)[0]==405;record('SSE GET honestly unsupported')
        status,reply,_=request(data=rpc('tools/call',params={'name':'vm_exec','arguments':{'id':'x','argv':['/bin/true']}}),session=observer)
        assert status==200 and reply['error']['code']==-32602;record('observe cannot execute')
        inputter=initialize('inputter')
        _,input_tools,_=request(data=rpc('tools/list'),principal='inputter',session=inputter)
        input_names=[t['name'] for t in input_tools['result']['tools']]
        assert 'vm_input_click' in input_names and 'vm_acquire_control' in input_names and not any(n in input_names for n in ['vm_exec','vm_start','vm_file_download_begin']);record('independent HTTPS input scope',input_names)
        for principal,session in [('observer',observer),('operator',operator)]:
            status,denied,_=request(data=rpc('tools/call',params={'name':'vm_input_click','arguments':{'id':'https-smoke'}}),principal=principal,session=session)
            assert status==200 and denied['error']['code']==-32602,(status,denied)
        record('observe and execute scopes cannot input')
        status,denied,_=request(data=rpc('tools/call',params={'name':'vm_input_click','arguments':{'id':'not-authorized'}}),principal='inputter',session=inputter)
        assert status==200 and denied['error']['code']==-32602;record('input scope preserves VM allowlist')
        status,routed,_=request(data=rpc('tools/call',params={'name':'vm_acquire_control','arguments':{'id':'https-smoke'}}),principal='inputter',session=inputter)
        assert status==200 and routed['result']['isError'] and 'VM not found' in routed['result']['content'][0]['text'];record('input principal routes to real daemon without fake VM success')
        assert request('DELETE',principal='inputter',session=inputter)[0]==204
        listed=tool(observer,'vm_list',{},principal='observer');record('HTTPS → stdio → daemon actual list',listed)
        if args.config:
            vm=json.loads(args.config.read_text());vm['id']='https-smoke';vm['serial_log']=str(tmp/'serial.log')
            subprocess.run([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),'create',json.dumps(vm)],check=True,stdout=subprocess.DEVNULL)
            hidden=dict(vm);hidden['id']='private-host-vm'
            subprocess.run([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),'create',json.dumps(hidden)],check=True,stdout=subprocess.DEVNULL)
            vm_created=True
            visible=tool(observer,'vm_list',{},principal='observer')
            assert len(visible)==1 and visible[0]['config']['id']=='https-smoke' and 'kernel' not in visible[0]['config'] and 'command_line' not in visible[0]['config'];record('VM allowlist and host metadata redaction',visible)
            denied=request(data=rpc('tools/call',params={'name':'vm_status','arguments':{'id':'private-host-vm'}}),principal='operator',session=operator)
            assert denied[0]==200 and denied[1]['error']['code']==-32602;record('unauthorized VM denied')
            denied=request(data=rpc('tools/call',params={'name':'vm_create','arguments':vm}),principal='operator',session=operator)
            assert denied[0]==200 and denied[1]['error']['code']==-32602;record('remote host path configuration denied')
            started=tool(operator,'vm_start',{'id':vm['id']});assert 'kernel' not in started['config'];record('start result redacts host paths')
            for n in range(100):
                try:info=tool(operator,'vm_guest_info',{'id':vm['id']},number=10+n);break
                except AssertionError:time.sleep(.1)
            else:raise TimeoutError('actual guest unavailable')
            record('actual ARM guest through verified TLS',info)
            if args.files:
                filer=initialize('filer')
                for session,principal in [(observer,'observer'),(operator,'operator')]:
                    denied=request(data=rpc('tools/call',params={'name':'vm_file_download_begin','arguments':{'id':vm['id'],'path':'anything'}}),principal=principal,session=session)
                    assert denied[0]==200 and denied[1]['error']['code']==-32602,denied
                blob=b'HTTPS binary\x00'+secrets.token_bytes(65000);filename='tls-'+secrets.token_hex(8);digest=hashlib.sha256(blob).hexdigest()
                upload=tool(filer,'vm_file_upload_begin',{'id':vm['id'],'path':filename,'size':len(blob),'sha256':digest},principal='filer')['result']['token']
                for offset in range(0,len(blob),32768):
                    tool(filer,'vm_file_upload_write',{'id':vm['id'],'token':upload,'offset':offset,'data':base64.b64encode(blob[offset:offset+32768]).decode()},principal='filer')
                assert tool(filer,'vm_file_upload_commit',{'id':vm['id'],'token':upload},principal='filer')['result']['published']
                download=tool(filer,'vm_file_download_begin',{'id':vm['id'],'path':filename},principal='filer')['result'];received=bytearray()
                while True:
                    chunk=tool(filer,'vm_file_download_read',{'id':vm['id'],'token':download['token'],'offset':len(received)},principal='filer')['result'];received.extend(base64.b64decode(chunk['data'],validate=True))
                    if chunk['eof']:break
                assert bytes(received)==blob and hashlib.sha256(received).hexdigest()==download['sha256']
                tool(filer,'vm_file_upload_begin',{'id':vm['id'],'path':'partial-'+secrets.token_hex(8),'size':3,'sha256':hashlib.sha256(b'abc').hexdigest()},principal='filer')
                assert request('DELETE',principal='filer',session=filer)[0]==204
                policy=tool(operator,'vm_guest_info',{'id':vm['id']})['result']['file_policy'];assert policy['active_transfers']==0 and policy['staged_upload_files']==0,policy
                record('independent files scope over verified HTTPS roundtrip and DELETE cleanup',{'bytes':len(blob),'sha256':digest,'policy':policy})
            record('HTTPS control lease',tool(operator,'vm_acquire_control',{'id':vm['id']}))
            executed=tool(operator,'vm_exec',{'id':vm['id'],'argv':['/bin/uname','-m'],'timeout_ms':5000});assert executed['result']['stdout']=='aarch64\n';record('HTTPS actual guest execution',executed)
            for closing in ['DELETE', 'idle-expiry']:
                unique=secrets.token_hex(8);ready=f'/tmp/veynode-http-sync-{unique}-ready';late=f'/tmp/veynode-http-sync-{unique}-late'
                delay=3 if closing=='DELETE' else 8
                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                    pending=pool.submit(tool,operator,'vm_exec',{'id':vm['id'],'argv':['/bin/sh','-c',f'printf ready > {ready}; (sleep {delay}; printf late > {late}) & wait'],'timeout_ms':15000})
                    time.sleep(.6)
                    action_started=time.monotonic()
                    old_sync=operator
                    if closing=='DELETE':
                        assert request('DELETE',principal='operator',session=operator)[0]==204
                    terminal=pending.result(timeout=10)['result']
                    assert terminal['cancelled'] and not terminal['timed_out'],terminal
                    elapsed=time.monotonic()-action_started
                    assert elapsed < (4 if closing=='DELETE' else 7),elapsed
                assert request(data=rpc('ping'),principal='operator',session=old_sync)[0]==404
                operator=initialize('operator');tool(operator,'vm_acquire_control',{'id':vm['id']})
                time.sleep(3.2)
                absent=tool(operator,'vm_exec',{'id':vm['id'],'argv':['/bin/sh','-c',f'test -f {ready} && test ! -e {late}']})
                assert absent['result']['exit_code']==0,absent
                record('HTTPS '+closing+' interrupts in-flight synchronous command',{'terminal':terminal,'elapsed_after_action_seconds':elapsed,'ready_present_late_absent':True})
            ready='/tmp/veynode-http-delete-ready';late='/tmp/veynode-http-delete-late'
            started=tool(operator,'vm_exec_start',{'id':vm['id'],'argv':['/bin/sh','-c',f'printf ready > {ready}; sleep 2; printf late > {late}'],'timeout_ms':10000})
            for n in range(50):
                probe=tool(operator,'vm_exec',{'id':vm['id'],'argv':['/bin/sh','-c',f'test -e {ready}']},number=200+n)
                if probe['result']['exit_code']==0:break
                time.sleep(.02)
            else:raise AssertionError('HTTP marker job never started')
            assert request('DELETE',principal='operator',session=operator)[0]==204
            operator=initialize('operator');record('DELETE releases control lease',tool(operator,'vm_acquire_control',{'id':vm['id']}))
            time.sleep(2.2)
            absent=tool(operator,'vm_exec',{'id':vm['id'],'argv':['/bin/sh','-c',f'test ! -e {late}']})
            assert absent['result']['exit_code']==0,absent;record('HTTPS DELETE cancels actual running guest job',{'job_id':started['result']['job_id'],'late_marker_absent':True})
        if args.config:
            ready='/tmp/veynode-http-expire-ready';late='/tmp/veynode-http-expire-late'
            tool(operator,'vm_exec_start',{'id':'https-smoke','argv':['/bin/sh','-c',f'printf ready > {ready}; sleep 8; printf late > {late}'],'timeout_ms':15000})
            for n in range(50):
                probe=tool(operator,'vm_exec',{'id':'https-smoke','argv':['/bin/sh','-c',f'test -f {ready}']},number=300+n)
                if probe['result']['exit_code']==0:break
                time.sleep(.02)
            else:raise AssertionError('expiry marker job did not start')
            expired_operator=operator;time.sleep(8.5)
            assert request(data=rpc('ping'),principal='operator',session=expired_operator)[0]==404
            operator=initialize('operator');tool(operator,'vm_acquire_control',{'id':'https-smoke'})
            absent=tool(operator,'vm_exec',{'id':'https-smoke','argv':['/bin/sh','-c',f'test ! -e {late}']})
            assert absent['result']['exit_code']==0,absent;record('HTTPS idle expiry cancels actual running job and releases lease',True)
        old=operator;assert request('DELETE',principal='operator',session=operator)[0]==204
        assert request(data=rpc('ping'),principal='operator',session=old)[0]==404;record('deleted session rejected')
        if args.sdk:
            import asyncio
            import importlib.metadata
            import httpx2
            from mcp.client.session import ClientSession
            from mcp.client.streamable_http import streamable_http_client
            async def official_sdk():
                async with httpx2.AsyncClient(verify=context,headers={'Authorization':'Bearer '+tokens['observer']},timeout=10) as http:
                    async with streamable_http_client(f'https://127.0.0.1:{port}/mcp',http_client=http) as (reader,writer):
                        async with ClientSession(reader,writer,read_timeout_seconds=10) as session:
                            initialized=await session.initialize()
                            tools=await session.list_tools()
                            listed=await session.call_tool('vm_list',{})
                            assert not listed.is_error
                            return {'sdk_version':importlib.metadata.version('mcp'),'negotiated_protocol':initialized.protocol_version,'tool_count':len(tools.tools)}
            record('official Python MCP SDK over verified HTTPS',asyncio.run(official_sdk()))
        if args.node_sdk:
            token_file=tmp/'node-sdk-token';token_file.write_text(tokens['operator']);token_file.chmod(0o600)
            command=['node',str(ROOT/'tests/mcp-client/acceptance.mjs'),'https',f'https://localhost:{port}/mcp',str(token_file)]
            if args.config:command.append('https-smoke')
            env=dict(os.environ);env['NODE_EXTRA_CA_CERTS']=str(cert)
            evidence=subprocess.check_output(command,env=env,timeout=45,text=True)
            record('official TypeScript MCP SDK over verified HTTPS',json.loads(evidence))
        expired=initialize('observer');time.sleep(6)
        assert request(data=rpc('ping'),session=expired)[0]==404;record('idle sessions expire')
        result['passed']=True
    except Exception as exc:
        result['passed']=False;result['error']=str(exc);raise
    finally:
        if vm_created:subprocess.run([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),'force_stop',json.dumps({'id':'https-smoke','confirm':'https-smoke'})],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=10)
        server.terminate();server.wait(timeout=5);daemon.terminate();daemon.wait(timeout=5)
        result['finished_at_unix']=time.time();args.output.parent.mkdir(parents=True,exist_ok=True)
        output=args.output if result.get('passed') else args.output.with_suffix('.failed.json')
        output.write_text(json.dumps(result,indent=2)+'\n')
print(f'HTTPS MCP checks passed; evidence: {args.output}')
