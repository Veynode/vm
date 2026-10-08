#!/usr/bin/env python3
"""Create a private macOS validation VM without replacing disks or identities."""
import argparse,json,os,pathlib,shutil,sys
ROOT=pathlib.Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser();p.add_argument('--restore-image',type=pathlib.Path,required=True);p.add_argument('--name',default='macos-smoke');p.add_argument('--disk-gib',type=int,default=64);p.add_argument('--cpus',type=int,default=4);p.add_argument('--memory-mib',type=int,default=4096);a=p.parse_args()
if not a.name or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_' for c in a.name): p.error('Name must use letters, digits, dash, underscore')
restore=a.restore_image.resolve(strict=True)
if not restore.is_file(): p.error('Restore image must be a file')
if not 32 <= a.disk_gib <= 256: p.error('disk-gib must be between 32 and 256')
if not 2 <= a.cpus <= 8 or not 4096 <= a.memory_mib <= 16384: p.error('Invalid validation VM CPU/memory allocation')
vm=ROOT/'.local/vms'/a.name
vm.mkdir(parents=True,mode=0o700,exist_ok=True);vm.chmod(0o700)
config=vm/'config.json'
if config.exists():
    existing=json.loads(config.read_text())
    if existing.get('restore_image')!=str(restore): p.error('Existing config refers to a different restore image')
    print(str(config));sys.exit(0)
disk=vm/'disk.raw'
fd=os.open(disk,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
try: os.ftruncate(fd,a.disk_gib*1024**3)
finally: os.close(fd)
identity=vm/'identity';identity.mkdir(mode=0o700)
share=ROOT/'.local/guest-share';share.mkdir(mode=0o700,exist_ok=True)
for name in ('metal-probe','h264-probe'):
    source=ROOT/'.local/bin'/name
    if source.is_file(): shutil.copy2(source,share/name)
value={'id':a.name,'guest':'macos','cpus':a.cpus,'memory_mib':a.memory_mib,'disk':str(disk),'restore_image':str(restore),'machine_identity_dir':str(identity),'window':False,'shared_dir':str(share),'width':2560,'height':1440}
fd=os.open(config,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
with os.fdopen(fd,'w') as f: json.dump(value,f,indent=2);f.write('\n')
print(str(config))
