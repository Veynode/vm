#!/usr/bin/env python3
"""Observe an already running, registered macOS Guest via actual MCP. Never starts/stops a VM."""
import argparse,base64,hashlib,importlib.util,json,os,subprocess
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('mcp_test',ROOT/'scripts/test-control-qemu.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',type=Path,default=ROOT/'target/debug/vm-control');p.add_argument('--state-dir',type=Path,required=True);p.add_argument('--id',required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
output=a.output.resolve();output.parent.mkdir(parents=True,exist_ok=True)
if output.exists() or output.with_suffix('.jpg').exists():raise SystemExit('Refusing evidence overwrite')
report={'passed':False,'checks':[],'controller_sha256':hashlib.sha256(a.binary.read_bytes()).hexdigest(),'scope':'actual registered macOS Guest, observe-only MCP adapter; never captures host desktop or changes VM lifecycle'}
def record(name,value):report['checks'].append({'name':name,'evidence':value})
c=module.MCP(a.binary.resolve(),a.state_dir)
try:
 names=[t['name'] for t in c.rpc('tools/list',{})['tools']];assert 'vm_screenshot' in names and not any(n in names for n in ['vm_exec','vm_start','vm_input_click']);record('observe_only_tools',names)
 info=c.tool('vm_guest_info',{'id':a.id});assert not info['isError'],info
 info=json.loads(info['content'][0]['text'])['result'];assert info['os']=='darwin' and info['machine'].startswith('VirtualMac') and info['capabilities']['screenshot'] is True;record('actual_guest_observation_available',info)
 frames=[]
 for n in range(2):
  reply=c.tool('vm_screenshot',{'id':a.id});assert not reply['isError'],reply
  image=reply['content'][0];meta=json.loads(reply['content'][1]['text']);data=base64.b64decode(image['data'],validate=True)
  assert image['type']=='image' and image['mimeType']=='image/jpeg' and meta['mime_type']=='image/jpeg'
  assert data.startswith(b'\xff\xd8\xff') and data.endswith(b'\xff\xd9') and len(data)==meta['byte_length']<=700*1024
  assert hashlib.sha256(data).hexdigest()==meta['sha256'] and meta['resized'] is False
  assert meta['width']==meta['image_width']==meta['source_width'] and meta['height']==meta['image_height']==meta['source_height']
  assert meta['jpeg_quality'] in [0.8,0.65,0.5,0.35] and meta['jpeg_quality_ladder']==[0.8,0.65,0.5,0.35]
  assert meta['point_pixel_scale']==meta['guest_ui_scale']>0 and meta['captured_at_us']>=meta['capture_requested_at_us']
  frames.append(meta)
  if n==0:
   image_path=output.with_suffix('.jpg');fd=os.open(image_path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
   with os.fdopen(fd,'wb') as f:f.write(data)
   decoded=json.loads(subprocess.check_output(['ffprobe','-v','error','-select_streams','v:0','-show_entries','stream=codec_name,width,height','-of','json',str(image_path)]))['streams'][0]
   assert decoded['codec_name']=='mjpeg' and decoded['width']==meta['width'] and decoded['height']==meta['height'];record('independent_jpeg_decode',decoded)
 assert frames[0]['frame_id']!=frames[1]['frame_id'] and frames[1]['captured_monotonic_us']>frames[0]['captured_monotonic_us'];record('actual_mcp_frames',frames);report['passed']=True
except Exception as error:report['error']=repr(error);raise
finally:
 c.close();output.write_text(json.dumps(report,indent=2)+'\n');output.chmod(0o600)
