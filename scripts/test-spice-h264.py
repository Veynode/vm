#!/usr/bin/env python3
"""Real SPICE desktop -> required hardware H264/HEVC -> MP4 -> FFmpeg decode evidence.

Uses the existing disposable 1536 MiB headless GPU desktop fixture. Correctness
probe only: it does not establish production WebRTC capability or performance.
"""
import argparse,datetime,hashlib,json,os,pathlib,shutil,subprocess,threading,time
os.umask(0o077)
ROOT=pathlib.Path(__file__).resolve().parent.parent
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--codec',choices=['h264','hevc'],default='h264')
parser.add_argument('--fixture-script',type=pathlib.Path,default=ROOT/'scripts/qemu-linux-desktop-probe.py',help='Compatible real desktop fixture entry point')
parser.add_argument('--fixture-arg',action='append',default=[],help='Extra single argument passed to fixture (use = for flags)')
parser.add_argument('--output',type=pathlib.Path,default=ROOT/'.local/evidence'/('spice-h264-'+datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'.json'))
a=parser.parse_args();output=a.output.resolve();output.parent.mkdir(parents=True,exist_ok=True)
if output.exists():raise SystemExit('Refusing to overwrite existing evidence')
fixture=output.with_suffix('.fixture.json');prefix=fixture.with_suffix('');encoded=pathlib.Path(str(prefix)+'-extra')
report={'timestamp_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'scope':'Actual first-consumer SPICE desktop capture, bounded hardware encode and local decoding only. Not WebRTC/browser, correct desktop rendering, reconnect, or performance acceptance.','vm_memory_mib':1536,'native_view_opened':False,'performance_measured':False,'reconnect_tested':False,'requested_codec':a.codec,'color_signaling_explicitly_configured':False}
try:
 subprocess.run([str(ROOT/'native/tools/build-spice-h264.sh')],check=True,cwd=ROOT)
 result=subprocess.run(['python3',str(a.fixture_script.resolve()),'--output',str(fixture),'--capture-cycles','0','--extra-consumer',str(ROOT/'.local/bin/spice-h264-probe')]+a.fixture_arg,env={**os.environ,'VEYNODE_VIDEO_CODEC':a.codec},cwd=ROOT,timeout=270,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
 pathlib.Path(str(prefix)+'.runner.log').write_text(result.stdout)
 report['fixture_exit_code']=result.returncode
 if fixture.exists():report['fixture']=json.loads(fixture.read_text())
 encode_path=pathlib.Path(str(encoded)+'.encode.json')
 if encode_path.exists():report['encode']=json.loads(encode_path.read_text())
 if result.returncode!=0:raise RuntimeError('Real desktop/encoding fixture failed; see runner/extra logs')
 encode=report['encode']
 if not encode['success'] or not encode['hardware_verified'] or encode['encoder_output_frames']!=encode['input_frames_submitted']:raise RuntimeError('Hardware encoder acceptance failed')
 mp4=pathlib.Path(str(encoded)+'.mp4');raw_bitstream=pathlib.Path(str(encoded)+'.'+a.codec)
 ffmpeg=shutil.which('ffmpeg');ffprobe=shutil.which('ffprobe')
 if not ffmpeg or not ffprobe:raise RuntimeError('FFmpeg/ffprobe required for independent actual decode validation')
 metadata=json.loads(subprocess.check_output([ffprobe,'-v','error','-count_frames','-select_streams','v:0','-show_entries','stream=codec_name,codec_tag_string,profile,width,height,nb_read_frames,pix_fmt,color_range,color_space,color_transfer,color_primaries','-of','json',str(mp4)],text=True,timeout=30))['streams'][0]
 for key in ['color_range','color_space','color_transfer','color_primaries']:metadata.setdefault(key,None)
 width,height=int(metadata['width']),int(metadata['height'])
 if metadata['codec_name']!=a.codec or (width,height)!=(encode['width'],encode['height']):raise RuntimeError('Encoded codec/dimensions mismatch')
 raw_metadata=json.loads(subprocess.check_output([ffprobe,'-v','error','-count_frames','-select_streams','v:0','-show_entries','stream=codec_name,width,height,nb_read_frames','-of','json',str(raw_bitstream)],text=True,timeout=30))['streams'][0]
 if raw_metadata['codec_name']!=a.codec or int(raw_metadata['nb_read_frames'])!=encode['encoder_output_frames'] or (int(raw_metadata['width']),int(raw_metadata['height']))!=(width,height):raise RuntimeError('Annex B independent decode or dimensions mismatch')
 size=width*height*3;hashes=[];nonblack=[];changed=[];first=None
 stderr_path=pathlib.Path(str(encoded)+'.decode.stderr.log')
 with stderr_path.open('x') as stderr:
  decoder=subprocess.Popen([ffmpeg,'-v','error','-noautorotate','-i',str(mp4),'-map','0:v:0','-vsync','0','-f','rawvideo','-pix_fmt','rgb24','pipe:1'],stdout=subprocess.PIPE,stderr=stderr)
  timer=threading.Timer(45,decoder.kill);timer.start()
  try:
   while True:
    frame=bytearray()
    while len(frame)<size:
     chunk=decoder.stdout.read(size-len(frame))
     if not chunk:break
     frame.extend(chunk)
    if not frame:break
    if len(frame)!=size:raise RuntimeError('Decoded partial frame')
    hashes.append(hashlib.sha256(frame).hexdigest())
    # Sparse 4x4 sampling bounds Python CPU while still checking real RGB pixels.
    samples=range(0,width*height,16)
    nonblack.append(sum(any(frame[p*3+c]>8 for c in range(3)) for p in samples))
    if first is None:first=bytes(frame)
    changed.append(sum(max(abs(frame[p*3+c]-first[p*3+c]) for c in range(3))>16 for p in range(0,width*height,16)))
   exit_code=decoder.wait(timeout=5)
   if exit_code!=0:raise RuntimeError('FFmpeg decoding failed: '+stderr_path.read_text()[:2000])
  finally:
   timer.cancel()
   if decoder.poll() is None:decoder.kill();decoder.wait()
 if len(hashes)!=encode['encoder_output_frames'] or len(hashes)!=int(metadata['nb_read_frames']):raise RuntimeError('Decode frame count mismatch')
 if min(nonblack)<100 or len(set(hashes))<2 or max(changed)<100:raise RuntimeError('Decoded frames must be nonblack with substantial real pixel differences')
 reference=pathlib.Path(str(encoded)+'.reference.bgra').read_bytes()
 if len(reference)!=width*height*4:raise RuntimeError('Normalized reference dimensions mismatch')
 if encode['normalization_gpu_copies']!=len(hashes) or encode['bitstream_orientation']!='top-left':raise RuntimeError('Every encoded frame must be normalized')
 squared=0;inverse_squared=0;sample_count=0
 for y in range(0,height,4):
  for x in range(0,width,4):
   p=y*width+x;inverse=(height-1-y)*width+x
   for c in range(3):
    squared+=(first[p*3+c]-reference[p*4+2-c])**2
    inverse_squared+=(first[p*3+c]-reference[inverse*4+2-c])**2
    sample_count+=1
 mse=squared/sample_count;inverse_mse=inverse_squared/sample_count
 if mse>=inverse_mse*0.2:raise RuntimeError('Decoded frame fails orientation correlation against normalized reference')
 reference_png=pathlib.Path(str(encoded)+'.reference.png');decoded_png=pathlib.Path(str(encoded)+'.decoded-first.png')
 subprocess.run([ffmpeg,'-v','error','-f','rawvideo','-pixel_format','bgra','-video_size',f'{width}x{height}','-i',str(encoded)+'.reference.bgra','-frames:v','1',str(reference_png)],check=True,timeout=20)
 subprocess.run([ffmpeg,'-v','error','-f','rawvideo','-pixel_format','rgb24','-video_size',f'{width}x{height}','-i','pipe:0','-frames:v','1',str(decoded_png)],input=first,check=True,timeout=20)
 guest_png=pathlib.Path(str(prefix)+'-guest.png')
 guest_rgb=subprocess.check_output([ffmpeg,'-v','error','-i',str(guest_png),'-f','rawvideo','-pix_fmt','rgb24','pipe:1'],timeout=20)
 if len(guest_rgb)!=size:raise RuntimeError('Guest XWD reference dimensions mismatch')
 def blue_centroid(rgb):
  count=0;total_x=0;total_y=0
  for y in range(0,height,2):
   for x in range(0,width,2):
    p=(y*width+x)*3;r,g,b=rgb[p:p+3]
    if b>80 and b>1.3*r and b>1.3*g:count+=1;total_x+=x;total_y+=y
  if count<100:raise RuntimeError('Actual Guest blue gear orientation marker missing')
  return {'x':total_x/count,'y':total_y/count,'sampled_pixels':count}
 guest_blue=blue_centroid(guest_rgb);decoded_blue=blue_centroid(first)
 if abs(guest_blue['y']-decoded_blue['y'])>height*0.1 or abs(guest_blue['x']-decoded_blue['x'])>width*0.1:raise RuntimeError('Decoded orientation differs from actual Guest XWD blue gear marker')
 report['orientation']={'guest_xwd_blue_centroid':guest_blue,'decoded_blue_centroid':decoded_blue,'guest_xwd_png':str(guest_png),'normalized_reference_png':str(reference_png),'decoded_first_png':str(decoded_png),'decoded_to_normalized_reference_mse':mse,'decoded_to_vertically_inverted_reference_mse':inverse_mse,'mse_ratio':mse/inverse_mse,'assertion':'Decoded bitstream must match normalized source at least 5x better than its vertical inverse; raw row mapping also independently matches actual Guest XWD blue-gear centroid.'}
 report.update(success=True,decode={'tool':subprocess.check_output([ffmpeg,'-version'],text=True).splitlines()[0],'metadata':metadata,'annex_b_metadata':raw_metadata,'decoded_frames':len(hashes),'unique_frame_hashes':len(set(hashes)),'frame_sha256':hashes,'pixel_sample_stride':16,'nonblack_samples_per_frame':nonblack,'samples_changed_by_more_than_16_from_first':changed,'orientation_note':'Actual pixels normalized by one Metal copy before compression; raw Annex B and MP4 are top-left without a track transform.'},artifacts={'mp4':str(mp4),'mp4_sha256':hashlib.sha256(mp4.read_bytes()).hexdigest(),a.codec:str(raw_bitstream),a.codec+'_sha256':hashlib.sha256(raw_bitstream.read_bytes()).hexdigest()})
except BaseException as error:
 report.update(success=False,error=str(error))
finally:
 output.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
raise SystemExit(0 if report.get('success') else 1)
