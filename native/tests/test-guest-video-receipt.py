#!/usr/bin/env python3
"""Actual host process/private Unix protocol; synthetic Guest, no capture/VM."""
import argparse, json, os, pathlib, select, socket, struct, subprocess, tempfile, time
ROOT=pathlib.Path(__file__).resolve().parents[2]
SERVICE='a'*32
CAPTURE='b'*32
IDENTITY={'version':1,'security_session_id':42,'uid':501,'console_set':None,'display_id':1,'physical_width':2560,'physical_height':1440,'display_geometry_sha256':'a'*64}
def packet(value):
 data=b'\1'+json.dumps(value).encode();return struct.pack('>I',len(data))+data
def meta():return {'service_instance':SERVICE,'capture_instance':CAPTURE,'cleanup_protocol_version':1,'cleanup_retention_ms':120000}
def start():return {'event':'started','id':'host-start','width':1280,'height':720,'pixel_format':'nv12-video-range','color_matrix':'ITU-R BT.709','color_space':'ITU-R BT.709','capture_dynamic_range':'SDR',**meta()}
def listener_at(path):
 listener=socket.socket(socket.AF_UNIX);listener.bind(str(path));path.chmod(0o600);listener.listen(1);listener.settimeout(3);return listener

def encoder_case(binary,mode):
 with tempfile.TemporaryDirectory(prefix='gr-',dir=ROOT/'.local/run') as raw:
  directory=pathlib.Path(raw);path=directory/'s';receipt=directory/'receipt.json';listener=listener_at(path)
  args=[str(binary),str(path),'--codec','h264','--cleanup-receipt',str(receipt)]
  if mode=='combined_identity':
   identity=directory/'identity.json';identity.write_text(json.dumps(IDENTITY));identity.chmod(0o600);args+=['--expected-console-identity',str(identity)]
  process=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE);conn=None;began=time.monotonic();prefix=b''
  try:
   conn,_=listener.accept();conn.settimeout(3);stream=conn.makefile('rb',buffering=0)
   request=json.loads(stream.readline(2049));assert request['cleanup_receipt'] is True
   assert receipt.exists() and receipt.stat().st_mode&0o777==0o600 and receipt.read_bytes()==b''
   registration={'event':'capture_registered','id':'host-start',**meta()}
   accepted=mode not in ['bad_service','boolean_version','bad_retention','wrong_request','started_first','pre_admission_error','replaced_file']
   if mode=='bad_service':registration['service_instance']='A'*32
   if mode=='boolean_version':registration['cleanup_protocol_version']=True
   if mode=='bad_retention':registration['cleanup_retention_ms']=120001
   if mode=='wrong_request':registration['id']='other'
   if mode=='replaced_file':receipt.unlink();receipt.write_text('replacement');receipt.chmod(0o600)
   if mode=='started_first':conn.sendall(packet(start()))
   elif mode=='pre_admission_error':conn.sendall(packet({'event':'error','id':'host-start','code':'busy'}));conn.shutdown(socket.SHUT_WR)
   else:conn.sendall(packet(registration))
   if accepted:
    deadline=time.monotonic()+1
    while True:
     try:saved=json.loads(receipt.read_bytes());break
     except json.JSONDecodeError:
      assert time.monotonic()<deadline,'registration must persist before started';time.sleep(.01)
    assert saved=={'version':1,'service_instance':SERVICE,'capture_instance':CAPTURE,'cleanup_retention_ms':120000}
    ack=start()
    if mode=='combined_identity':ack.update(console_identity=IDENTITY,console_binding_mode='expected_identity_matched',frame_console_identity_scope='this_capture_connection')
    if mode=='foreign_started':ack['capture_instance']='c'*32
    if mode=='duplicate_registration':conn.sendall(packet(registration))
    elif mode in ['foreign_terminated','error_clean']:
     error={'event':'error','id':'host-start','code':'console_identity_mismatch',**meta()}
     terminal={'event':'terminated','id':'host-start','reason':'capture_failed','cleanup_confirmed':True,'cleanup_basis':'no_stream_created',**meta()}
     if mode=='foreign_terminated':terminal['capture_instance']='c'*32
     conn.sendall(packet(error)+packet(terminal));conn.shutdown(socket.SHUT_WR)
    else:
     conn.sendall(packet(ack))
     if mode=='registered_frame':
      body=struct.pack('>BQQIIII',2,1,1000000000,1280,720,1280*720,1280*720//2)+bytes([80])*(1280*720)+bytes([128])*(1280*720//2)
      conn.sendall(struct.pack('>I',len(body))+body)
      deadline=time.monotonic()+3
      while len(prefix)<13:
       assert select.select([process.stdout],[],[],max(0,deadline-time.monotonic()))[0], 'registered hardware frame deadline'
       part=os.read(process.stdout.fileno(),13-len(prefix));assert part;prefix+=part
      assert 0<struct.unpack('>I',prefix[:4])[0]<=4*1024*1024 and prefix[12]==1

   if mode not in ['pre_admission_error','foreign_terminated','error_clean']:
    if mode in ['normal','combined_identity','registered_frame','lost_ack','foreign_stopped']:
     time.sleep(.03);process.stdin.write(b'stop\n');process.stdin.flush()
    stop=json.loads(stream.readline(2049));assert stop=={'id':'host-stop','method':'stop'},stop
    if mode!='lost_ack':
     stopped={'event':'stopped','id':'host-stop',**meta()}
     if mode=='foreign_stopped':stopped['service_instance']='d'*32
     conn.sendall(packet(stopped))
    conn.shutdown(socket.SHUT_WR)
   out,err=process.communicate(timeout=4);summary=json.loads(err.splitlines()[-1]);assert (bool(prefix+out) and summary['encoded']>=1 and summary['hardware_verified'] is True) if mode=='registered_frame' else (out==b'' and summary['encoded']==0)
   expected=0 if mode in ['normal','combined_identity','registered_frame'] else 78 if mode=='error_clean' else 1
   assert process.returncode==expected,(mode,process.returncode,summary)
   assert summary['cleanup_receipt_persisted'] is accepted,(mode,summary)
   if mode in ['lost_ack','foreign_stopped','foreign_terminated','pre_admission_error','started_first','bad_service','boolean_version','bad_retention','wrong_request','replaced_file']:assert summary['guest_cleanup_confirmed'] is False,(mode,summary)
   if not accepted:assert receipt.read_bytes()==(b'replacement' if mode=='replaced_file' else b'')
   return {'mode':mode,'exit':process.returncode,'receipt_persisted':accepted,'cleanup_confirmed':summary['guest_cleanup_confirmed'],'elapsed_ms':round((time.monotonic()-began)*1000)}
  finally:
   if process.poll() is None:process.kill();process.wait(timeout=2)
   if conn:conn.close()
   listener.close()

def query_case(binary,mode):
 with tempfile.TemporaryDirectory(prefix='gq-',dir=ROOT/'.local/run') as raw:
  path=pathlib.Path(raw)/'s';listener=listener_at(path)
  process=subprocess.Popen([str(binary),'--query-cleanup',str(path),'--service-instance',SERVICE,'--capture-instance',CAPTURE],stdout=subprocess.PIPE,stderr=subprocess.PIPE);conn=None;began=time.monotonic()
  try:
   conn,_=listener.accept();conn.settimeout(3);stream=conn.makefile('rb',buffering=0)
   request=json.loads(stream.readline(2049));assert request=={'id':'host-cleanup-query','method':'cleanup_status','service_instance':SERVICE,'capture_instance':CAPTURE}
   reply={'event':'cleanup_status','id':'host-cleanup-query','protocol_version':1,'service_instance':SERVICE,'capture_instance':CAPTURE,'status':'clean','cleanup_confirmed':True,'retention_ms':120000,'cleanup_basis':'stop_capture_completed'}
   if mode in ['active','failed','unknown','service_mismatch']:
    reply.update(status=mode,cleanup_confirmed=False);del reply['cleanup_basis']
    if mode=='service_mismatch':reply['service_instance']='c'*32
   elif mode in ['no_stream_created','delegate_reported_stopped']:reply['cleanup_basis']=mode
   elif mode=='foreign_service':reply['service_instance']='c'*32
   elif mode=='foreign_capture':reply['capture_instance']='c'*32
   elif mode=='wrong_id':reply['id']='other'
   elif mode=='false_clean':reply['cleanup_confirmed']=False
   elif mode=='numeric_true':reply['cleanup_confirmed']=1
   elif mode=='unknown_basis':reply['cleanup_basis']='any_stop'
   elif mode=='missing_basis':del reply['cleanup_basis']
   elif mode=='wrong_retention':reply['retention_ms']=119999
   elif mode=='extra_secret':reply['secret']='/private/SECRET'
   elif mode=='fake_service_mismatch':reply.update(status='service_mismatch',cleanup_confirmed=False);del reply['cleanup_basis']
   elif mode=='active_claims_clean':reply['status']='active'
   data=packet(reply)
   if mode=='wrong_kind':data=data[:4]+b'\2'+data[5:]
   if mode=='two_records':data+=data
   if mode=='trailing_byte':data+=b'x'
   if mode=='partial':data=data[:12]
   if mode=='oversize':data=struct.pack('>I',3000)+b'x'*3000
   conn.sendall(data)
   if mode not in ['no_eof','partial']:conn.shutdown(socket.SHUT_WR)
   out,err=process.communicate(timeout=5);result=json.loads(out);clean=mode in ['clean','no_stream_created','delegate_reported_stopped']
   assert process.returncode==(0 if clean else 1),(mode,process.returncode,result)
   assert result['proof'] is clean and result['cleanup_confirmed'] is clean,(mode,result)
   if mode in ['active','failed','unknown','service_mismatch']:assert result['status']==mode
   assert b'SECRET' not in out and len(out)<=2048 and err==b''
   return {'mode':mode,'exit':process.returncode,'status':result['status'],'proof':clean,'elapsed_ms':round((time.monotonic()-began)*1000)}
  finally:
   if process.poll() is None:process.kill();process.wait(timeout=2)
   if conn:conn.close()
   listener.close()

def bad_path(binary,mode):
 with tempfile.TemporaryDirectory(prefix='gp-',dir=ROOT/'.local/run') as raw:
  directory=pathlib.Path(raw);receipt=directory/'receipt';original=b'original'
  if mode in ['exists','hardlink','symlink']:
   receipt.write_bytes(original);receipt.chmod(0o600)
   if mode=='hardlink':os.link(receipt,directory/'link')
   if mode=='symlink':link=directory/'link';link.symlink_to(receipt);receipt=link
  if mode=='parent_permissions':directory.chmod(0o755)
  if mode=='parent_symlink':
   real=directory/'real';real.mkdir(mode=0o700);link=directory/'link';link.symlink_to(real);receipt=link/'receipt'
  if mode=='fifo':os.mkfifo(receipt,0o600)
  process=subprocess.run([str(binary),str(directory/'no-socket'),'--codec','h264','--cleanup-receipt',str(receipt)],capture_output=True,timeout=2)
  assert process.returncode==2 and process.stdout==b'',(mode,process.returncode,process.stderr)
  if mode in ['exists','hardlink','symlink']:assert receipt.read_bytes()==original
  return {'invalid_path':mode,'exit':2}

def main():
 p=argparse.ArgumentParser();p.add_argument('--binary',type=pathlib.Path,required=True);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
 report={'scope':'actual host executable/private Unix synthetic Guest; no host or Guest capture','passed':False,'cases':[]}
 with a.output.open('x') as output:
  try:
   assert json.loads(subprocess.check_output([str(a.binary),'--capabilities']))['cleanup_receipt_version']==1
   for mode in ['normal','combined_identity','registered_frame','lost_ack','foreign_started','foreign_stopped','foreign_terminated','duplicate_registration','error_clean','bad_service','boolean_version','bad_retention','wrong_request','started_first','pre_admission_error','replaced_file']:
    report['cases'].append(encoder_case(a.binary,mode));print('PASS encoder',mode,flush=True)
   for mode in ['clean','no_stream_created','delegate_reported_stopped','active','failed','unknown','service_mismatch','foreign_service','foreign_capture','wrong_id','false_clean','numeric_true','unknown_basis','missing_basis','wrong_retention','extra_secret','fake_service_mismatch','active_claims_clean','wrong_kind','two_records','trailing_byte','partial','no_eof','oversize']:
    report['cases'].append(query_case(a.binary,mode));print('PASS query',mode,flush=True)
   for mode in ['exists','hardlink','symlink','parent_permissions','parent_symlink','fifo']:
    report['cases'].append(bad_path(a.binary,mode));print('PASS path',mode,flush=True)
   report['passed']=True
  finally:json.dump(report,output,indent=2);output.write('\n')
if __name__=='__main__':main()
