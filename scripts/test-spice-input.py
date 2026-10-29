#!/usr/bin/env python3
"""RAM-only real virgl console physical-key/click proof, or no-device control.

No product permission/lease, Unicode, keyboard-layout or browser claim. The
Guest X11 oracle independently checks real events and resulting window pixels.
"""
import argparse,base64,hashlib,importlib.util,json,os,pathlib,shlex,subprocess,sys,tempfile,time
ROOT=pathlib.Path(__file__).resolve().parents[1]
BASE=ROOT/'.local/runtime/utm'
def load(path,name):
 spec=importlib.util.spec_from_file_location(name,path);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--no-devices',action='store_true');p.add_argument('--coordinates',action='store_true');p.add_argument('--coordinate-transport',choices=['spice','qmp'],default='spice');p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
 if a.coordinate_transport=='qmp' and not a.coordinates:p.error('QMP option requires coordinate probe')
 if a.coordinates and a.no_devices:p.error('Coordinate probe needs devices')
 host_source=ROOT/('native/tools/spice-coordinate-probe.c' if a.coordinates else 'native/tools/spice-input-probe.c')
 guest_source=ROOT/('scripts/linux-input-coordinates.c' if a.coordinates else 'scripts/linux-input-oracle.c')
 output=a.output.resolve();output.parent.mkdir(parents=True,exist_ok=True)
 if output.exists():raise SystemExit('Refusing evidence overwrite')
 headers=BASE/'CocoaSpice-d8d29fc810047a3ddcebb351cdadc8fe4b4f308d/Sources/CocoaSpice/ExternalHeaders';frameworks=BASE/'UTM.app/Contents/Frameworks';binary=BASE/('spice-coordinate-probe' if a.coordinates else 'spice-input-probe')
 subprocess.run(['clang','-Wall','-Wextra',*[f'-I{headers/s}' for s in ['', 'glib-2.0','spice-client-glib-2.0','spice-1']],f'-F{frameworks}',f'-Wl,-rpath,{frameworks}','-framework','spice-client-glib-2.0.8','-framework','glib-2.0.0','-framework','gobject-2.0.0','-framework','gio-2.0.0',str(host_source),'-o',str(binary)],check=True)
 fixture=load(ROOT/'scripts/qemu-linux-desktop-probe.py','input_fixture')
 modloop=ROOT/'.local/media/alpine-3.24.2/modloop-virt';version='6.18.52-0-virt'
 deps=dict(line.split(':',1) for line in subprocess.check_output(['unsquashfs','-cat',str(modloop),f'modules/{version}/modules.dep'],text=True).splitlines());modules=set()
 def include(name):
  if name in modules:return
  modules.add(name)
  for dep in deps[name].split():include(dep)
 include('kernel/drivers/virtio/virtio_input.ko')
 include('kernel/drivers/input/evdev.ko')
 inject=''
 for name in sorted(modules):
  data=subprocess.check_output(['unsquashfs','-cat',str(modloop),f'modules/{version}/{name}']);target=f'/usr/lib/modules/{version}/{name}'
  inject+=f'mkdir -p {shlex.quote(str(pathlib.PurePosixPath(target).parent))}\nbase64 -d > {shlex.quote(target)} <<\'B64MODULE\'\n{base64.b64encode(data).decode()}\nB64MODULE\n'
 init=fixture.INIT.replace('modprobe virtio_gpu',inject+'modprobe virtio_gpu\nmodprobe virtio_input\nmodprobe evdev\ncat /proc/bus/input/devices')
 init=init.replace('apk add --no-cache --no-progress','mount -t tmpfs -o remount,size=1100M rootfs /\napk add --no-cache --no-progress').replace('font-dejavu eudev','font-dejavu eudev build-base libx11-dev')
 init=init.replace('Option "AutoAddDevices" "false"','Option "AutoAddDevices" "true"')
 if a.coordinates:init=init.replace('build-base libx11-dev','build-base libx11-dev linux-headers xinput')
 source=guest_source.read_bytes()
 init=init.replace('twm &',f"base64 -d > /veynode/input-oracle.c <<'B64ORACLE'\n{base64.b64encode(source).decode()}\nB64ORACLE\ncc -Wall -Wextra -O2 /veynode/input-oracle.c -o /veynode/input-oracle -lX11\n"+'twm &')
 init=init.replace('sleep 2\nkill -0 "$gears"','sleep 2\n/veynode/input-oracle &\nsleep 1\nkill -0 "$gears"')
 if a.coordinates:init=init.replace('/veynode/input-oracle &','xinput list --long\n/veynode/input-oracle &')
 fixture.INIT=init
 qmp_sent=[]
 if a.coordinates and a.coordinate_transport=='qmp':
  original_qmp=fixture.QMP
  class CoordinateQMP(original_qmp):
   def run(self,command,arguments=None):
    reply=super().run(command,arguments)
    if command=='screendump':
     points=[(320,600),(0,0),(1,1),(2,2),(200,200),(639,399),(640,400),(1278,798),(1279,799),(0,799),(1279,0),(320,600)]
     for index,(x,y) in enumerate(points):
      # Center of the desired Guest pixel in its measured inclusive evdev range.
      ax=(2*x+1)*32768//(2*1280);ay=(2*y+1)*32768//(2*800)
      super().run('input-send-event',{'events':[{'type':'abs','data':{'axis':'x','value':ax}},{'type':'abs','data':{'axis':'y','value':ay}}]})
      qmp_sent.append({'index':index,'x':x,'y':y,'abs_x':ax,'abs_y':ay});time.sleep(.35)
    return reply
  fixture.QMP=CoordinateQMP
 original_popen=subprocess.Popen
 def popen(command,*args,**kwargs):
  if isinstance(command,list) and '-nodefaults' in command and not a.no_devices:command=command+['-device','virtio-keyboard-pci','-device','virtio-tablet-pci']
  return original_popen(command,*args,**kwargs)
 subprocess.Popen=popen
 fixture_path=output.with_suffix('.fixture.json');prefix=fixture_path.with_suffix('')
 try:
  with tempfile.TemporaryDirectory(prefix='veynode-input-consumer-') as temp:
   consumer=pathlib.Path(temp)/'consumer.sh'
   send_command='' if a.coordinates and a.coordinate_transport=='qmp' else shlex.quote(str(binary))+' "$1" "$2"\n'
   pause='32' if a.coordinates and a.coordinate_transport=='qmp' else ('25' if a.coordinates else '20')
   consumer.write_text('#!/bin/sh\nset -eu\n'+send_command+'sleep '+pause+'\n'+shlex.quote(str(BASE/'spice-capture'))+' "$1" "$2"\n');consumer.chmod(0o700)
   sys.argv=[str(fixture.__file__),'--output',str(fixture_path),'--capture-cycles','0','--extra-consumer',str(consumer),'--extra-consumer-timeout','75']
   status=fixture.main()
 finally:subprocess.Popen=original_popen
 serial=fixture_path.with_suffix('.serial.log').read_text(errors='replace');result_lines=[line.split('INPUT_RESULT ',1)[1] for line in serial.splitlines() if 'INPUT_RESULT ' in line]
 observed=json.loads(result_lines[-1]) if result_lines else None
 result={'coordinate_probe':a.coordinates,'no_devices_control':a.no_devices,'fixture_exit':status,'guest_input':observed,'events':[line for line in serial.splitlines() if 'INPUT_EVENT' in line],'input_source_sha256':hashlib.sha256(source).hexdigest(),'host_source_sha256':hashlib.sha256(host_source.read_bytes()).hexdigest(),'modules':sorted(modules),'scope':'Physical key A under default Xorg US mapping; absolute console coordinates. No Unicode, product authorization, Retina, keyboard-layout or browser acceptance.'}
 if a.coordinates:
  sent_log=pathlib.Path(str(prefix)+'-extra.log').read_text()
  def records(text,marker):return [json.loads(line.split(marker,1)[1]) for line in text.splitlines() if marker in line]
  sent=qmp_sent if a.coordinate_transport=='qmp' else records(sent_log,'COORD_SENT ');motions=records(serial,'COORD_MOTION ');raw=records(serial,'COORD_RAW ')
  result.update(sent=sent,motions=motions,evdev=raw,device_metadata=[line for line in serial.splitlines() if line.startswith(('COORD_DEVICE ','COORD_AXIS '))])
  predicted_raw=[{'x':p['abs_x'],'y':p['abs_y']} if a.coordinate_transport=='qmp' else {'x':p['x']*32767//1280,'y':p['y']*32767//800} for p in sent]
  predicted_motion=[{'x':p['x']*1280//32768,'y':p['y']*800//32768,'synthetic':0} for p in predicted_raw]
  result['coordinate_transport']=a.coordinate_transport
  result['model_raw_matches']=raw==predicted_raw
  result['model_motion_matches']=motions==predicted_motion
  result['exact_requested_pixels']=len(sent)==len(motions) and all((s['x'],s['y'])==(m['x'],m['y']) for s,m in zip(sent,motions))
  result['success']=status==0 and observed is not None and len(sent)==12 and result['model_raw_matches'] and result['model_motion_matches'] and (a.coordinate_transport!='qmp' or result['exact_requested_pixels'])
  result['scope']='Mapping diagnostic; observed errors are not corrected or accepted as product coordinate support.'
  result['fixture_source_sha256']=hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest()
  output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));return 0 if result['success'] else 1
 expected=observed is not None and ((observed['clicks']==0 and observed['keys']==0 and not observed['passed']) if a.no_devices else observed['passed'])
 png=pathlib.Path(str(prefix)+'-extra-80.png')
 pixels=subprocess.check_output(['ffmpeg','-v','error','-i',str(png),'-f','rawvideo','-pix_fmt','rgb24','pipe:1'],timeout=15)
 if len(pixels)!=1280*800*3:raise RuntimeError('Unexpected capture dimensions')
 samples=[list(pixels[(y*1280+x)*3:(y*1280+x)*3+3]) for x,y in [(120,120),(320,120)]]
 host_changed=samples[0][1]>240 and samples[0][0]<20 and samples[0][2]<20 and samples[1][0]>240 and samples[1][1]<20 and samples[1][2]<20
 host_ok=(samples[0]==samples[1] and not host_changed) if a.no_devices else host_changed
 result['host_capture']={'path':str(png),'samples_rgb':samples,'oracle_positions':[[120,120],[320,120]],'expected_pixel_assertion':host_ok}
 result['fixture_source_sha256']=hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest()
 result['success']=status==0 and expected and host_ok
 output.write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2));return 0 if result['success'] else 1
if __name__=='__main__':raise SystemExit(main())
