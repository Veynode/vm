#!/usr/bin/env python3
"""Host encoder identity protocol tests. Synthetic NV12; never screen capture/VM."""
import argparse, copy, json, os, pathlib, select, socket, struct, subprocess, tempfile, time
ROOT=pathlib.Path(__file__).resolve().parents[2]
IDENTITY={'version':1,'security_session_id':42,'uid':501,'console_set':None,'display_id':1,'physical_width':2560,'physical_height':1440,'display_geometry_sha256':'a'*64}
def packet(v):
 b=b'\1'+json.dumps(v).encode();return struct.pack('>I',len(b))+b

def run(binary,mode):
 with tempfile.TemporaryDirectory(prefix='gb-',dir=ROOT/'.local/run') as d:
  d=pathlib.Path(d);path=d/'s';identity=d/'identity.json';identity.write_text(json.dumps(IDENTITY));identity.chmod(0o600)
  listener=socket.socket(socket.AF_UNIX);listener.bind(str(path));path.chmod(0o600);listener.listen(1);listener.settimeout(3)
  args=[str(binary),str(path),'--codec','h264']
  if mode!='legacy':args+=['--expected-console-identity',str(identity)]
  p=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE);conn=None;began=time.monotonic()
  try:
   conn,_=listener.accept();conn.settimeout(3);stream=conn.makefile('rb',buffering=0)
   request=json.loads(stream.readline(4097));assert request['method']=='start'
   assert (request.get('expected_console_identity')==IDENTITY) if mode!='legacy' else ('expected_console_identity' not in request)
   ack={'event':'started','id':'host-start','width':1280,'height':720,'pixel_format':'nv12-video-range','color_matrix':'ITU-R BT.709','color_space':'ITU-R BT.709','capture_dynamic_range':'SDR','console_identity':copy.deepcopy(IDENTITY),'console_binding_mode':'expected_identity_matched','frame_console_identity_scope':'this_capture_connection'}
   good=mode in ('matched','matched-frame','legacy');prefix=b''
   if mode=='legacy':
    for k in ['console_identity','console_binding_mode','frame_console_identity_scope']:del ack[k]
   elif mode=='missing':del ack['console_identity']
   elif mode=='null':ack['console_identity']=None
   elif mode=='foreign-uid':ack['console_identity']['uid']=502
   elif mode=='foreign-session':ack['console_identity']['security_session_id']=43
   elif mode=='foreign-geometry':ack['console_identity']['display_geometry_sha256']='b'*64
   elif mode=='boolean-version':ack['console_identity']['version']=True
   elif mode=='unknown-field':ack['console_identity']['untrusted']=1
   elif mode=='missing-console-set':del ack['console_identity']['console_set']
   elif mode=='wrong-mode':ack['console_binding_mode']='observe_unbound'
   elif mode=='wrong-scope':ack['frame_console_identity_scope']='other_connection'
   elif mode=='mismatch-queued-frame':ack['console_identity']['uid']=502
   elif mode=='guest-reject-clean':
    conn.sendall(packet({'event':'error','id':'host-start','code':'console_identity_mismatch'})+packet({'event':'terminated','id':'host-start','reason':'capture_failed','cleanup_confirmed':True,'cleanup_basis':'no_stream_created'}));conn.shutdown(socket.SHUT_WR)
   if mode!='guest-reject-clean':
    conn.sendall(packet(ack))
    if mode in ('mismatch-queued-frame','matched-frame'):
     body=struct.pack('>BQQIIII',2,1,1000000000,1280,720,1280*720,1280*720//2)+bytes([80])*(1280*720)+bytes([128])*(1280*720//2)
     conn.sendall(struct.pack('>I',len(body))+body)
     if mode=='matched-frame':
      deadline=time.monotonic()+3
      while len(prefix)<13:
       assert select.select([p.stdout],[],[],max(0,deadline-time.monotonic()))[0], 'bounded matched hardware output'
       part=os.read(p.stdout.fileno(),13-len(prefix));assert part;prefix+=part
      assert 0<struct.unpack('>I',prefix[:4])[0]<=4*1024*1024 and prefix[12]==1
    if good:
     time.sleep(.05);p.stdin.write(b'stop\n');p.stdin.flush()
    stop=json.loads(stream.readline(4097));assert stop=={'id':'host-stop','method':'stop'},stop
    conn.sendall(packet({'event':'stopped','id':'host-stop'}));conn.shutdown(socket.SHUT_WR)
   out,err=p.communicate(timeout=4);events=[json.loads(l) for l in err.splitlines()];result=events[-1]
   assert (bool(prefix+out) and result['encoded']>=1 and result['hardware_verified'] is True) if mode=='matched-frame' else (out==b'' and result['encoded']==0),(mode,result)
   assert p.returncode==(0 if good else 78 if mode=='guest-reject-clean' else 1),(mode,p.returncode,result)
   assert result['guest_cleanup_confirmed'] is True,(mode,result)
   assert result['expected_console_identity_matched']==(mode in ('matched','matched-frame')),(mode,result)
   assert result['console_identity_binding_requested']==(mode!='legacy')
   return {'mode':mode,'exit_code':p.returncode,'cleanup_confirmed':True,'encoded':result['encoded'],'elapsed_ms':round((time.monotonic()-began)*1000,2)}
  finally:
   if p.poll() is None:p.kill();p.wait(timeout=2)
   if conn:conn.close()
   listener.close()

def invalid_file(binary,mode):
 with tempfile.TemporaryDirectory(prefix='bi-',dir=ROOT/'.local/run') as d:
  d=pathlib.Path(d);f=d/'identity';value=copy.deepcopy(IDENTITY)
  if mode=='boolean':value['uid']=True
  if mode=='negative':value['console_set']=-1
  if mode=='reserved-session':value['security_session_id']=4294967294
  if mode=='float':value['physical_width']=1.5
  if mode=='overflow':value['display_id']=4294967296
  if mode=='extra':value['extra']=1
  f.write_text('x'*2049 if mode=='oversize' else json.dumps(value));f.chmod(0o644 if mode=='permissions' else 0o600)
  if mode=='symlink':g=d/'link';g.symlink_to(f);f=g
  if mode=='hardlink':os.link(f,d/'alias')
  if mode=='fifo':f.unlink();os.mkfifo(f,0o600)
  p=subprocess.run([str(binary),str(d/'no-socket'),'--codec','h264','--expected-console-identity',str(f)],capture_output=True,timeout=2)
  assert p.returncode==2 and not p.stdout,(mode,p.returncode)
  assert b'Expected console identity must be strict' in p.stderr,(mode,p.stderr)
  return {'invalid_file':mode,'exit_code':p.returncode}

def main():
 a=argparse.ArgumentParser();a.add_argument('--binary',type=pathlib.Path,required=True);a.add_argument('--output',type=pathlib.Path,required=True);args=a.parse_args()
 report={'scope':'synthetic protocol/Unix sockets; no Guest or host capture','passed':False,'cases':[]}
 with args.output.open('x') as f:
  try:
   caps=json.loads(subprocess.check_output([str(args.binary),'--capabilities']));assert caps['console_identity_binding_version']==1
   for mode in ['matched','matched-frame','legacy','missing','null','foreign-uid','foreign-session','foreign-geometry','boolean-version','unknown-field','missing-console-set','wrong-mode','wrong-scope','mismatch-queued-frame','guest-reject-clean']:
    report['cases'].append(run(args.binary,mode));print('PASS',mode,flush=True)
   for mode in ['boolean','negative','reserved-session','float','overflow','extra','oversize','permissions','symlink','hardlink','fifo']:
    report['cases'].append(invalid_file(args.binary,mode));print('PASS invalid',mode,flush=True)
   report['passed']=True
  finally:json.dump(report,f,indent=2);f.write('\n')
if __name__=='__main__':main()
