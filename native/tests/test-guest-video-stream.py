#!/usr/bin/env python3
"""Synthetic raw NV12 protocol -> actual required-hardware VT -> independent FFmpeg.
No VM, ScreenCaptureKit, desktop capture or network beyond private Unix sockets.
"""
import argparse, hashlib, json, os, pathlib, shutil, socket, struct, subprocess, tempfile, threading, time
ROOT=pathlib.Path(__file__).resolve().parents[2]
ENCODER=ROOT/'.local/bin/guest-video-stream'

def packet(kind,body):
    body=bytes([kind])+body
    return struct.pack('>I',len(body))+body

def event(**value):return packet(1,json.dumps(value).encode())

def pixels(w,h,index,noisy=False):
    if noisy:
        # Deterministic incompressible input exercises stdout backpressure.
        block=bytes((i*73+(i//251)*37)%256 for i in range(65536))
        return (block*((w*h*3//2+len(block)-1)//len(block)))[:w*h*3//2]
    y=40+(index%6)*30
    return bytes([y])*(w*h)+bytes([90+index*8,180-index*7])*(w*h//4)

def frame(w,h,seq,pts,index=0,noisy=False):
    return packet(2,struct.pack('>QQIIII',seq,pts,w,h,w*h,w*h//2)+pixels(w,h,index,noisy))

def exact(stream,n):
    data=b''
    while len(data)<n:
        chunk=stream.read(n-len(data))
        if not chunk:raise EOFError(f'partial frame: {len(data)}/{n}')
        data+=chunk
    return data

def run_case(out,mode,codec='h264'):
    report={'mode':mode,'codec':codec,'capture_performed':False}
    with tempfile.TemporaryDirectory(prefix='gv-',dir=ROOT/'.local/run') as directory:
        folder=pathlib.Path(directory);path=folder/'s';listener=socket.socket(socket.AF_UNIX)
        listener.bind(str(path));os.chmod(path,0o600);listener.listen(1);listener.settimeout(5)
        stderr_path=out/f'{mode}-{codec}.stderr'
        with stderr_path.open('wb') as stderr:
            process=subprocess.Popen([str(ENCODER),str(path),'--codec',codec],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr)
            conn=None;thread=None;records=[];reader_errors=[];stop_request=None
            def reader():
                try:
                    while True:
                        header=process.stdout.read(13)
                        if not header:return
                        if len(header)<13:header+=exact(process.stdout,13-len(header))
                        length,pts,key=struct.unpack('>IQB',header)
                        assert 0<length<=4*1024*1024 and key in (0,1),(length,key)
                        records.append((pts,key,exact(process.stdout,length)))
                        if mode=='broken-output':return
                except Exception as error:reader_errors.append(str(error))
            def wait_records(n,seconds=8):
                deadline=time.monotonic()+seconds
                while len(records)<n and process.poll() is None and time.monotonic()<deadline:time.sleep(.01)
                assert len(records)>=n,(mode,len(records),stderr_path.read_text())
            try:
                conn,_=listener.accept();conn.settimeout(5);commands=conn.makefile('rb',buffering=0)
                start=json.loads(commands.readline(4097));w,h=start['width'],start['height']
                assert start=={'id':'host-start','method':'start','width':w,'height':h,'fps':30},start
                assert (w,h)==((1920,1080) if codec=='hevc' else (1280,720))
                report['geometry']=[w,h]
                if mode!='blocked-output':thread=threading.Thread(target=reader,daemon=True);thread.start()
                ack=event(event='started',id='host-start',width=w,height=h,pixel_format='nv12-video-range',color_matrix='ITU-R BT.709',color_space='ITU-R BT.709',capture_dynamic_range='SDR')
                if mode=='frame-before-start':conn.sendall(frame(w,h,1,100))
                elif mode=='bad-length':conn.sendall(struct.pack('>I',33+1920*1080*3//2+1))
                elif mode=='bad-color':conn.sendall(event(event='started',id='host-start',width=w,height=h,pixel_format='nv12-video-range',color_matrix='BT.601',color_space='ITU-R BT.709',capture_dynamic_range='SDR'))
                elif mode=='bad-start':conn.sendall(event(event='started',id='host-start',width=w+2,height=h,pixel_format='nv12-video-range',color_matrix='ITU-R BT.709',color_space='ITU-R BT.709',capture_dynamic_range='SDR'))
                else:
                    conn.sendall(ack)
                    if mode=='bad-geometry':conn.sendall(frame(w+2,h,1,100))
                    elif mode=='truncated-plane':
                        raw=frame(w,h,1,100);conn.sendall(struct.pack('>I',len(raw)-5)+raw[4:-1])
                    elif mode=='zero-sequence':conn.sendall(frame(w,h,0,100))
                    elif mode=='zero-length':conn.sendall(struct.pack('>I',0))
                    elif mode=='unknown-kind':conn.sendall(packet(3,b'bad'))
                    elif mode=='source-eof':
                        conn.sendall(frame(w,h,1,100)[:100]);conn.shutdown(socket.SHUT_WR)
                    elif mode=='blocked-output':
                        # No stdout consumer; high-frequency luma/chroma is large
                        # enough to saturate the actual OS pipe on the first AU.
                        conn.sendall(frame(w,h,1,100,noisy=True))
                    else:
                        conn.sendall(frame(w,h,1,100,index=0));wait_records(1)
                        if mode=='duplicate-sequence':conn.sendall(frame(w,h,1,200,index=1))
                        elif mode=='backward-pts':conn.sendall(frame(w,h,2,100,index=1))
                        elif mode in ('missing-stop-ack','wrong-stop-ack','broken-output','bad-stdin'):
                            if mode=='broken-output':
                                thread.join(timeout=2);assert not thread.is_alive()
                                process.stdout.close();conn.sendall(frame(w,h,2,200,index=1))
                            else:
                                process.stdin.write(b'unknown\n' if mode=='bad-stdin' else b'stop\n');process.stdin.flush()
                        elif mode=='positive':
                            for index in range(1,6):
                                conn.sendall(frame(w,h,index+1,100+index*33333333,index=index));wait_records(index+1)
                            process.stdin.write(b'keyframe\n');process.stdin.flush();wait_records(7)
                            assert records[6][1]==1,'explicit keyframe from cached frame'
                            wait_records(8,3) # actual idle encode from retained NV12
                            process.stdin.write(b'stop\n');process.stdin.flush()
                        else:raise AssertionError(mode)
                stop_begin=time.monotonic()
                stop_request=json.loads(commands.readline(4097))
                assert stop_request=={'id':'host-stop','method':'stop'},stop_request
                if mode=='missing-stop-ack':process.wait(timeout=3)
                else:
                    try:conn.sendall(event(event='stopped',id='wrong' if mode=='wrong-stop-ack' else 'host-stop'))
                    except BrokenPipeError:
                        if mode=='positive':raise
                        report['ack_write_peer_already_closed']=True
                returncode=process.wait(timeout=5)
                if thread:thread.join(timeout=2)
                report.update(exit_code=returncode,stop_wait_seconds=time.monotonic()-stop_begin,records=len(records))
            finally:
                if process.poll() is None:process.kill();process.wait(timeout=3)
                if process.stdin:process.stdin.close()
                if conn:conn.close()
                listener.close()
                if thread:thread.join(timeout=2)
        diagnostics=[]
        for line in stderr_path.read_text().splitlines():
            try:diagnostics.append(json.loads(line))
            except json.JSONDecodeError:pass
        assert diagnostics,(mode,stderr_path.read_text())
        report['summary']=diagnostics[-1];report['reader_errors']=reader_errors
        if mode=='positive':
            assert report['exit_code']==0 and report['summary']['guest_cleanup_confirmed'],report
            assert report['summary']['hardware_verified'] and report['summary']['submitted']>=8,report
            assert not reader_errors,reader_errors
            assert records[0][0]==0 and records[0][1]==1
            assert all(b[0]>a[0] for a,b in zip(records,records[1:]))
            elementary=out/f'{mode}-{codec}.annexb';elementary.write_bytes(b''.join(r[2] for r in records))
            stream=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-of','json',str(elementary)],timeout=10))['streams'][0]
            for key in ['color_space','color_transfer','color_primaries']:assert stream.get(key)=='bt709',stream
            assert stream.get('color_range')=='tv',stream
            report['decoded_color_metadata']={key:stream[key] for key in ['color_range','color_space','color_transfer','color_primaries']}
            decoded=out/f'{mode}-{codec}.nv12'
            subprocess.run(['ffmpeg','-v','error','-i',str(elementary),'-f','rawvideo','-pix_fmt','nv12','-y',str(decoded)],check=True,timeout=15)
            raw=decoded.read_bytes();size=w*h*3//2
            assert len(raw)==size*len(records),(len(raw),size,len(records))
            hashes=[hashlib.sha256(raw[i*size:(i+1)*size]).hexdigest() for i in range(len(records))]
            # Independent pixel oracle: lossy reconstructed Y must remain near the
            # six deliberately different uniform input luminances.
            means=[sum(raw[i*size:i*size+w*h])/(w*h) for i in range(6)]
            assert all(abs(actual-(40+i*30))<4 for i,actual in enumerate(means)),means
            chroma=[(sum(raw[i*size+w*h:(i+1)*size:2])/(w*h//4),sum(raw[i*size+w*h+1:(i+1)*size:2])/(w*h//4)) for i in range(6)]
            assert all(abs(u-(90+i*8))<4 and abs(v-(180-i*7))<4 for i,(u,v) in enumerate(chroma)),chroma
            assert len(set(hashes[:6]))==6,hashes
            report.update(first_pts_us=records[0][0],pts_us=[r[0] for r in records],keyframes=[i for i,r in enumerate(records) if r[1]],decoded_frames=len(hashes),decoded_unique_first_six=len(set(hashes[:6])),decoded_luma_means=means,decoded_chroma_means=chroma)
        else:
            assert report['exit_code']!=0,report
            assert report['summary']['event']=='error',report
            if mode in ('missing-stop-ack','wrong-stop-ack'):assert not report['summary']['guest_cleanup_confirmed'],report
            if mode in ('blocked-output','broken-output'):
                assert 'stdout' in report['summary']['error'],report
                assert report['stop_wait_seconds']<2,report
            if mode=='missing-stop-ack':assert 1.3<report['stop_wait_seconds']<2.5,report
        return report

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--output',type=pathlib.Path,default=ROOT/'.local/evidence/guest-video-synthetic');args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=False);(ROOT/'.local/run').mkdir(exist_ok=True)
    reports=[]
    try:
        for codec in ['h264','hevc']:
            reports.append(run_case(args.output,'positive',codec));print('PASS actual hardware encode/decode',codec,flush=True)
        for mode in ['bad-length','zero-length','unknown-kind','bad-color','bad-start','frame-before-start','bad-geometry','truncated-plane','zero-sequence','duplicate-sequence','backward-pts','missing-stop-ack','wrong-stop-ack','blocked-output','broken-output','source-eof','bad-stdin']:
            reports.append(run_case(args.output,mode));print('PASS negative',mode,flush=True)
    finally:
        (args.output/'result.json').write_text(json.dumps({'scope':'synthetic NV12 input; actual VideoToolbox hardware and independent FFmpeg decode; no Guest capture','reports':reports},indent=2)+'\n')
if __name__=='__main__':main()
