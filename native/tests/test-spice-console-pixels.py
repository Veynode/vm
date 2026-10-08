#!/usr/bin/env python3
"""Independently decode saved actual persistent-console artifacts and compare pixels."""
import json,pathlib,subprocess,sys
prefix=sys.argv[1];output=pathlib.Path(sys.argv[2])
def pixels(path,w,h):return subprocess.check_output(['ffmpeg','-v','error','-i',path,'-frames:v','1','-vf',f'scale={w}:{h}:flags=neighbor','-f','rawvideo','-pix_fmt','rgb24','-'],timeout=15)
def blue(rgb,w,h):
 xs=ys=count=0
 for y in range(0,h,2):
  for x in range(0,w,2):
   r,g,b=rgb[(y*w+x)*3:(y*w+x)*3+3]
   if b>80 and b>1.3*r and b>1.3*g:xs+=x;ys+=y;count+=1
 if count<100:raise RuntimeError('Blue gear marker missing')
 return {'x':xs/count/w,'y':ys/count/h}
guest=blue(pixels(prefix.rsplit('-extra',1)[0]+'-guest.png',1280,800),1280,800)
report={'scope':'Saved actual Guest XWD, owned screenshot and independently decoded static epochs; not a performance benchmark','passed':False,'guest_blue_centroid':guest,'codecs':{}}
for codec,w,h in [('hevc',1280,800),('h264',1152,720)]:
 reference=pixels(prefix+'.png',w,h);decoded=pixels(prefix+'.'+codec,w,h);stride=w*3
 normal=sum((a-b)**2 for a,b in zip(reference,decoded))/len(reference)
 flipped=b''.join(reference[y*stride:(y+1)*stride] for y in range(h-1,-1,-1))
 inverse=sum((a-b)**2 for a,b in zip(flipped,decoded))/len(reference);marker=blue(decoded,w,h)
 if normal>=500 or normal*10>=inverse:raise RuntimeError('Decoded pixel error/orientation differs from owned reference')
 if max(abs(guest[k]-marker[k]) for k in ['x','y'])>.05:raise RuntimeError('Decoded orientation differs from independent Guest XWD')
 report['codecs'][codec]={'reference':'Same generation/sequence owned screenshot','decoded_first_frame_mse':normal,'vertically_inverted_mse':inverse,'decoded_blue_centroid':marker,'width':w,'height':h,'passed':True}
report['passed']=True;output.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
