#!/usr/bin/env python3
"""Actual fixed-user Linux files → daemon → scoped MCP, plus permission/boundary/cleanup negatives."""
import argparse,base64,hashlib,importlib.util,json,os,subprocess,tempfile,time,uuid
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('sync_client',ROOT/'scripts/test-control-sync-cancel.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);MCP=module.MCP
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--config',type=Path,required=True);p.add_argument('--output',type=Path,default=ROOT/'.local/evidence/control-files.json');p.add_argument('--expiry',action='store_true',help='Wait for actual60second idle cleanup');args=p.parse_args()
config=json.loads(args.config.read_text());assert Path(config['disk']).resolve().is_relative_to(ROOT/'.local/vms');assert config['memory_mib']<=1024
config['id']='files-validation';evidence={'started_at_unix':time.time(),'checks':[],'scope':'One actual1GiB installed Linux; dedicated files UID1001, execution UID1000; single filenames only','config_path':str(args.config.resolve())}
def record(name,value):evidence['checks'].append({'name':name,'evidence':value})
with tempfile.TemporaryDirectory(prefix='vm-files-') as directory:
 state=Path(directory)/'state';config['serial_log']=str(Path(directory)/'serial.log');clients=[]
 daemon=subprocess.Popen([str(ROOT/'target/debug/vm-control'),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
 def local(method,value):return json.loads(subprocess.check_output([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),method,json.dumps(value)],text=True,timeout=40))
 def client(scopes):
  c=MCP(state,scopes);clients.append(c);return c
 def tool(c,name,denied=False,**payload):return c.tool('vm_'+name,{'id':config['id'],**payload},denied=denied)
 def begin(c,name,data):return tool(c,'file_upload_begin',path=name,size=len(data),sha256=hashlib.sha256(data).hexdigest())['result']['token']
 def append(c,token,data):
  for offset in range(0,len(data),32768):assert tool(c,'file_upload_write',token=token,offset=offset,data=base64.b64encode(data[offset:offset+32768]).decode())['result']['offset']==min(len(data),offset+32768)
 def policy():return tool(observer,'guest_info')['result']['file_policy']
 def no_partial():
  for _ in range(100):
   found=policy()
   if found.get('active_transfers')==0 and found.get('staged_upload_files')==0:return found
   time.sleep(.05)
  raise AssertionError(('transfer cleanup incomplete',found))
 try:
  for _ in range(100):
   if (state/'control.sock').exists():break
   assert daemon.poll() is None;time.sleep(.05)
  local('create',config);life=client(['lifecycle']);observer=client([]);files=client(['files']);executor=client(['execute']);other=client(['files'])
  tool(life,'start')
  for _ in range(100):
   try:info=tool(observer,'guest_info')['result'];break
   except AssertionError:time.sleep(.1)
  else:raise AssertionError('Guest integration unavailable')
  assert info['capabilities']['files'] and info['file_policy']['uid']==1001,info
  assert info['execution_identity']['uid']==1000,info
  record('actual separated Guest identities and explicit file root',info)
  for unauthorized in [observer,executor,life]:tool(unauthorized,'file_download_begin',denied=True,path='x')
  tool(files,'exec',denied=True,argv=['/bin/true'])
  tool(files,'file_upload_begin',denied=True,path='owner-injection',size=0,sha256=hashlib.sha256(b'').hexdigest(),owner='a'*64)
  record('files permission independent of observe/lifecycle/execute; session owner cannot be supplied',True)
  body=b'\x00\xff'+('你好 ✓\n'.encode()*10000)+os.urandom(8192);name='roundtrip-'+uuid.uuid4().hex+'.bin';token=begin(files,name,body)
  tool(other,'file_upload_write',denied=True,token=token,offset=0,data=base64.b64encode(body[:10]).decode())
  tool(files,'file_download_begin',denied=True,path=name)
  append(files,token,body);committed=tool(files,'file_upload_commit',token=token)['result'];assert committed['published'] and committed['sha256']==hashlib.sha256(body).hexdigest(),committed
  tool(files,'file_upload_begin',denied=True,path=name,size=0,sha256=hashlib.sha256(b'').hexdigest())
  download=tool(files,'file_download_begin',path=name)['result'];readtoken=download['token'];output=bytearray()
  tool(other,'file_download_read',denied=True,token=readtoken,offset=0)
  while True:
   chunk=tool(files,'file_download_read',token=readtoken,offset=len(output))['result'];output.extend(base64.b64decode(chunk['data'],validate=True));assert chunk['offset']==len(output)
   if chunk['eof']:break
  assert bytes(output)==body and hashlib.sha256(output).hexdigest()==download['sha256'];record('binary Unicode multichunk atomic upload/download and checksum',{'bytes':len(body),'sha256':download['sha256'],'filename':name})
  record('other file sessions cannot use tokens; incomplete upload invisible; existing file not overwritten',True)
  for path in ['../outside','/etc/passwd','outside-link','outside-link/','outside-hardlink','root-owned','fifo','.veynode-staging','sub/file']:
   tool(files,'file_download_begin',denied=True,path=path)
  record('directory escape, symlink, hardlink, wrong owner, FIFO and reserved paths rejected',True)
  tool(files,'file_upload_begin',denied=True,path='too-large',size=8388609,sha256='0'*64)
  token=begin(files,'chunk-'+uuid.uuid4().hex,b'abcdef')
  tool(files,'file_upload_write',denied=True,token=token,offset=1,data=base64.b64encode(b'a').decode())
  tool(files,'file_upload_write',denied=True,token=token,offset=0,data=base64.b64encode(b'x'*32769).decode())
  tool(files,'file_upload_write',denied=True,token=token,offset=0,data='not-base64!')
  assert tool(files,'file_cancel',token=token)['result']['cancelled'];record('total/chunk/offset/base64 bounds and explicit cancellation',no_partial())
  bad='bad-hash-'+uuid.uuid4().hex;token=tool(files,'file_upload_begin',path=bad,size=3,sha256='0'*64)['result']['token'];append(files,token,b'abc');tool(files,'file_upload_commit',denied=True,token=token);tool(files,'file_download_begin',denied=True,path=bad);record('wrong SHA256 cannot publish and staging removed',no_partial())
  token=begin(files,'disconnect-'+uuid.uuid4().hex,b'partial-body');tool(files,'file_upload_write',token=token,offset=0,data=base64.b64encode(b'partial').decode());files.close();record('MCP EOF cleans actual partial file and closes ownership',no_partial())
  tool(other,'file_upload_commit',denied=True,token=token)
  binary=tool(executor,'exec',argv=['sha256sum','/usr/local/bin/veynode-guest'])['result'];expected_binary=hashlib.sha256((args.config.parent/'veynode-guest').read_bytes()).hexdigest();assert binary['exit_code']==0 and binary['stdout'].split()[0]==expected_binary,binary;record('actual installed Guest binary matches fixture build artifact',expected_binary)
  denied=tool(executor,'exec',argv=['/bin/sh','-c','cat /home/veynode-files/transfers/"$1"; touch /home/veynode-files/transfers/execute-must-not-write','test',name])['result'];assert denied['exit_code']!=0 and 'Permission denied' in denied['stderr'],denied
  record('execution UID1000 cannot read or change dedicated files directory',denied)
  if args.expiry:
   token=begin(other,'expiry-'+uuid.uuid4().hex,b'partial');tool(other,'file_upload_write',token=token,offset=0,data=base64.b64encode(b'p').decode());before=policy();assert before['staged_upload_files']==1,before
   deadline=time.monotonic()+62
   while time.monotonic()<deadline:time.sleep(min(1,deadline-time.monotonic()))
   tool(other,'file_upload_commit',denied=True,token=token);record('actual60second idle expiration removes partial file',no_partial())
  assert tool(life,'shutdown',mode='guest')['result']['requested']
  for _ in range(150):
   if local('status',{'id':config['id']})['state']=='stopped':break
   time.sleep(.1)
  else:raise AssertionError('normal shutdown did not complete')
  record('normal Guest shutdown',True);evidence['passed']=True
 except Exception as error:evidence.update(passed=False,error=str(error));raise
 finally:
  for c in clients:
   try:c.close()
   except Exception as error:evidence.setdefault('client_cleanup_errors',[]).append(str(error))
  try:
   if local('status',{'id':config['id']})['state']!='stopped':evidence['forced_failure_cleanup']=local('force_stop',{'id':config['id'],'confirm':config['id']})
  finally:
   daemon.terminate();daemon.wait(timeout=10);evidence['finished_at_unix']=time.time();args.output.parent.mkdir(parents=True,exist_ok=True);output=args.output if evidence.get('passed') else args.output.with_suffix('.failed.json');output.write_text(json.dumps(evidence,indent=2)+'\n');print(output)
