#!/usr/bin/env python3
"""Real read-only macOS Guest windows via official MCP SDK and local verified TLS.

Never changes VM lifecycle, Guest services/TCC, OS trust or window state. Starts
only an owned loopback HTTPS adapter. Temporary keys/tokens/session ids/raw
window metadata are not saved in evidence. No screenshot is requested.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import socket
import ssl
import subprocess
import tempfile
import time

spec=importlib.util.spec_from_file_location('window_https_support',Path(__file__).with_name('test-macos-files-http.py'))
common=importlib.util.module_from_spec(spec);spec.loader.exec_module(common)
common.ORIGIN='https://veynode-windows-test.invalid'
check,Failure=common.check,common.Failure
ROOT=Path(__file__).resolve().parents[1]
SDK=ROOT/'tests/mcp-client/node_modules/@modelcontextprotocol/sdk'

# Imported by an ephemeral module, so no generated source or credentials enters
# the repository. Only allowlisted counts/booleans/digests reach stdout.
JAVASCRIPT=r'''
import {readFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {createHash} from 'node:crypto';
const [sdk,url,tokenFile,vm,origin]=process.argv.slice(2);
const {Client}=await import(pathToFileURL(sdk+'/dist/esm/client/index.js'));
const {StreamableHTTPClientTransport}=await import(pathToFileURL(sdk+'/dist/esm/client/streamableHttp.js'));
class GateError extends Error {}
function check(ok,code){if(!ok)throw new GateError(code);}
const token=readFileSync(tokenFile,'utf8').trim();
const transport=new StreamableHTTPClientTransport(new URL(url),{requestInit:{headers:{Authorization:`Bearer ${token}`,Origin:origin}}});
const client=new Client({name:'veynode-macos-window-https',version:'1'},{capabilities:{}});
const result={passed:false,client:'official @modelcontextprotocol/sdk 1.32.1',snapshots:[]};
let timer=setTimeout(()=>{process.stderr.write('official_sdk_deadline\n');process.exit(2)},35000);
try {
  await client.connect(transport);result.initialize=true;
  const tools=await client.listTools();
  const names=tools.tools.map(t=>t.name);
  check(names.includes('vm_windows')&&names.includes('vm_guest_info'),'observe_tools_missing');
  check(!names.some(n=>n.startsWith('vm_input_')||n.startsWith('vm_file_')||n.startsWith('vm_exec')||['vm_start','vm_force_stop'].includes(n)),'observe_scope_expanded');
  result.observe_tools_only=true;
  async function call(name,args){
    const response=await client.callTool({name,arguments:args});
    check(response.isError!==true&&response.content.length===1&&response.content[0].type==='text','tool_failed');
    return JSON.parse(response.content[0].text);
  }
  const info=(await call('vm_guest_info',{id:vm})).result;
  check(info.os==='darwin'&&info.machine.startsWith('VirtualMac')&&info.uid>0&&info.capabilities.windows===true,'actual_guest_windows_unavailable');
  check(info.capabilities.input===false,'unexpected_input_permission');
  result.guest={os:info.os,windows:true,input:false};
  let previous;
  for(let i=0;i<2;i++){
    const start=performance.now();const s=await call('vm_windows',{id:vm});
    check(s.version===1&&s.coordinate_space==='guest-desktop-global-points'&&s.titles_included===false&&s.accessibility_tree===false&&s.input_authorization===false,'snapshot_semantics');
    check(Array.isArray(s.windows)&&s.windows.length<=128&&Array.isArray(s.displays)&&s.displays.length>0&&s.displays.length<=16,'snapshot_limits');
    check(Buffer.byteLength(JSON.stringify(s))<=49152,'snapshot_bytes');
    const allowed=['window_id','owner','bounds_points','display_ids'].sort().join(',');
    for(const w of s.windows){
      check(Object.keys(w).sort().join(',')===allowed,'private_window_fields');
      check(Object.keys(w.owner).sort().join(',')===['pid','bundle_identifier','application_name'].sort().join(','),'private_owner_fields');
      check(Buffer.byteLength(w.owner.bundle_identifier)<=256&&Buffer.byteLength(w.owner.application_name)<=256,'owner_limit');
    }
    check(!previous||s.snapshot_id!==previous.snapshot_id,'snapshot_id_reused');
    check(!previous||s.completed_monotonic_us>=previous.completed_monotonic_us,'observation_clock_regressed');
    result.snapshots.push({window_count:s.windows.length,display_count:s.displays.length,
      truncated:s.truncated,eligible_window_count:s.eligible_window_count,omitted_invalid_count:s.omitted_invalid_count,
      titles_included:false,input_authorization:false,accessibility_tree:false,
      response_sha256:createHash('sha256').update(JSON.stringify(s)).digest('hex'),elapsed_ms:Math.round(performance.now()-start)});
    previous=s;
  }
  await transport.terminateSession();result.delete_acknowledged=true;
  await client.close();result.client_closed=true;result.passed=true;
} catch(error){result.error=error instanceof GateError?error.message:'official_sdk_acceptance_failed';process.exitCode=1;}
finally {clearTimeout(timer);await client.close();console.log(JSON.stringify(result));}
'''


def write_private(path,value):
    with os.fdopen(common.support.private_new(path),'w') as stream:
        stream.write(value);stream.flush();os.fsync(stream.fileno())


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('server','control','state-dir','output'):parser.add_argument('--'+name,type=Path,required=True)
    parser.add_argument('--id',required=True)
    args=parser.parse_args()
    check(all(p.is_absolute() for p in (args.server,args.control,args.state_dir,args.output)),'absolute_paths_required')
    check(all(p.is_file() and not p.is_symlink() for p in (args.server,args.control)),'frozen_binaries_required')
    check(args.state_dir.is_dir() and not args.state_dir.is_symlink(),'existing_state_required')
    check(json.loads((SDK/'package.json').read_text())['version']=='1.32.1','pinned_official_sdk_required')
    report_fd=common.support.private_new(args.output)
    report={'schema':1,'passed':False,'started_at_unix':time.time(),'checks':[],
            'server_sha256':common.digest(args.server),'controller_sha256':common.digest(args.control),
            'script_sha256':common.digest(Path(__file__)),
            'scope':'Real macOS read-only windows over loopback verified HTTPS; official SDK positive path, direct HTTP negative gates; no VM/TCC/window/input changes',
            'tls':'Temporary CA trusted only by Python context and Node process; certificate and hostname verification enabled; no OS trust changes',
            'credentials_persisted':False,'raw_window_metadata_persisted':False}
    server=http=None;phase='setup'
    def record(name,evidence=True):
        report['checks'].append({'name':name,'evidence':evidence});print(name,flush=True)
    try:
        with tempfile.TemporaryDirectory(prefix='veynode-windows-https-') as directory:
            private=Path(directory);private.chmod(0o700)
            cert,key,config=[private/n for n in ('cert.pem','key.pem','config.json')]
            subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-sha256','-keyout',str(key),'-out',str(cert),'-days','1','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost,IP:127.0.0.1'],check=True,timeout=15,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            cert.chmod(0o600);key.chmod(0o600)
            tokens={name:secrets.token_urlsafe(32) for name in ('observer','other-observer')}
            with socket.socket() as bound:bound.bind(('127.0.0.1',0));port=bound.getsockname()[1]
            configuration={'bind':f'127.0.0.1:{port}','tls_cert':str(cert),'tls_key':str(key),'allowed_origins':[common.ORIGIN],
                'session_idle_seconds':5,'principals':[{'name':name,'token_sha256':hashlib.sha256(token.encode()).hexdigest(),
                    'vm_ids':[args.id],'scopes':['observe']} for name,token in tokens.items()]}
            write_private(config,json.dumps(configuration))
            command=[str(args.server),'--config',str(config),'--state-dir',str(args.state_dir),'--control-bin',str(args.control)]
            server=subprocess.Popen(command,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            http=common.HTTPS(port,cert,tokens);until=time.monotonic()+8
            while True:
                check(server.poll() is None,'server_exited_before_ready')
                try:
                    if http.request('GET',principal=None)[0]==401:break
                except (ConnectionRefusedError,ConnectionResetError):pass
                check(time.monotonic()<until,'server_ready_deadline');time.sleep(.05)
            phase='tls_auth'
            conn=common.http_client.HTTPSConnection('127.0.0.1',port,context=ssl.create_default_context(),timeout=3)
            try:
                try:conn.request('GET','/mcp');conn.getresponse()
                except ssl.SSLCertVerificationError:record('untrusted certificate rejected')
                else:raise Failure('untrusted_ca_accepted')
            finally:conn.close()
            check(http.rpc('ping',principal=None)[0]==401,'missing_auth_accepted')
            check(http.rpc('ping',principal='invalid')[0]==401,'invalid_auth_accepted')
            check(http.request('GET',principal='observer',extra={'Origin':'https://foreign.invalid'})[0]==403,'foreign_origin_accepted')
            record('missing/invalid bearer and foreign Origin rejected')
            # Product requires observe on every principal. Validate wrong-scope
            # configuration separately, never claim a no-observe session existed.
            invalid={**configuration,'principals':[{**configuration['principals'][0],'scopes':['files']}]}
            invalid_path=private/'invalid.json';write_private(invalid_path,json.dumps(invalid))
            denied=subprocess.run([str(args.server),'--config',str(invalid_path),'--state-dir',str(args.state_dir),'--control-bin',str(args.control)],capture_output=True,timeout=5)
            check(denied.returncode!=0 and b'invalid or duplicate principal definition' in denied.stderr,'observe_required_config_gate')
            record('principal without observe rejected before server admission')
            phase='official_sdk'
            script=private/'client.mjs';token_path=private/'token';write_private(script,JAVASCRIPT);write_private(token_path,tokens['observer'])
            env={k:v for k,v in os.environ.items() if k not in ('NODE_TLS_REJECT_UNAUTHORIZED','NODE_OPTIONS','NODE_EXTRA_CA_CERTS')}
            env['NODE_EXTRA_CA_CERTS']=str(cert)
            sdk=subprocess.run(['node',str(script),str(SDK),f'https://localhost:{port}/mcp',str(token_path),args.id,common.ORIGIN],env=env,capture_output=True,timeout=40)
            check(len(sdk.stdout)<=65536 and len(sdk.stderr)<=65536,'sdk_output_bound')
            try:result=json.loads(sdk.stdout)
            except Exception:raise Failure('official_sdk_missing_safe_report') from None
            report['official_sdk']=result
            check(sdk.returncode==0 and result.get('passed') is True,'official_sdk_failed')
            record('official SDK initialize/list/real Guest info/two windows/DELETE/client close',result)
            phase='session_policy'
            sid=http.initialize('observer')
            check(http.rpc('ping',principal='other-observer',session=sid)[0]==404,'cross_principal_session_accepted')
            status,reply,_=http.rpc('tools/call',{'name':'vm_windows','arguments':{'id':'unauthorized-vm'}},'observer',sid)
            check(status==200 and reply.get('error',{}).get('code')==-32602,'VM_allowlist_not_enforced')
            status,reply,_=http.rpc('tools/call',{'name':'vm_windows','arguments':{'id':args.id,'include_titles':True}},'observer',sid)
            check(status==200 and reply.get('error',{}).get('code')==-32602,'title_argument_accepted')
            record('cross-principal session, other VM and title parameter rejected')
            http.delete('observer',sid)
            check(http.rpc('ping',principal='observer',session=sid)[0]==404,'deleted_session_reusable')
            record('DELETE cleanup acknowledged and session cannot be reused')
            sid=http.initialize('observer');started=time.monotonic();time.sleep(6.2)
            check(http.rpc('ping',principal='observer',session=sid)[0]==404,'expired_session_reusable')
            http.delete('observer',sid,expired=True)
            record('5-second idle session expires',{'observed_after_seconds':round(time.monotonic()-started,3)})
            sid=http.initialize('observer')
            server.terminate();code=server.wait(timeout=45)
            check(code==0,'server_signal_cleanup_unconfirmed');http.sessions.clear()
            record('owned HTTPS SIGTERM with active observe session reconciled and reaped',{'exit_code':code})
            check(common.digest(args.server)==report['server_sha256'] and common.digest(args.control)==report['controller_sha256'],'frozen_binary_changed')
            report['passed']=True
    except Exception as error:
        report['error']={'phase':phase,'code':str(error) if isinstance(error,Failure) else 'acceptance_operation_failed'}
    finally:
        if server is not None and server.poll() is None:
            if http is not None:
                for principal,sid in list(http.sessions):
                    try:http.delete(principal,sid,expired=True)
                    except Exception:report['http_cleanup_unconfirmed']=True;report['passed']=False
            server.terminate()
            try:server.wait(timeout=45)
            except subprocess.TimeoutExpired:
                server.kill();server.wait(timeout=5);report['forced_kill']=True;report['passed']=False
        if server is not None:
            report['server_exit_code']=server.returncode
            if server.returncode!=0:report['passed']=False
        report['finished_at_unix']=time.time()
        with os.fdopen(report_fd,'w') as out:json.dump(report,out,indent=2);out.write('\n');out.flush();os.fsync(out.fileno())
    print(json.dumps({'passed':report['passed'],'checks':len(report['checks'])}),flush=True)
    return 0 if report['passed'] else 1

if __name__=='__main__':
    try:raise SystemExit(main())
    except Exception:print('HTTPS windows acceptance setup failed; no protocol data emitted.',flush=True);raise SystemExit(1)
