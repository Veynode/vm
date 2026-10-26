#!/usr/bin/env python3
"""SIGKILL/restart actual Guest service on an existing owned fixed fixture disk.

The diagnostic root controller is injected in a temporary initramfs; installed
service files/binary remain unchanged. Root harness actions are not guest RPCs.
"""
import argparse, gzip, importlib.util, json, os
from pathlib import Path
import signal, subprocess, tempfile, time
from cgroup_test_support import MCP
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--config',type=Path,required=True);args=parser.parse_args()
config=json.loads(args.config.read_text())
if not Path(config['disk']).resolve().is_relative_to(ROOT/'.local/vms'):raise SystemExit('Only owned repository fixture disk accepted')
MEDIA=ROOT/'.local/media/alpine-3.24.2';OUT=ROOT/'.local/evidence/cgroup-recovery';OUT.mkdir(parents=True,exist_ok=True)
spec=importlib.util.spec_from_file_location('fixture',ROOT/'scripts/build-linux-fixture.py');fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
controller=r'''#!/bin/sh
set -eu
trap 'echo CGROUP_RECOVERY_ROOT_FAIL; poweroff -f' EXIT
rm -f /home/veynode/recovery-ready /home/veynode/recovery-marker /home/veynode/recovery-complete /home/veynode/recovery-finish
agent() {
 /usr/local/bin/veynode-guest --allow-execute --allow-shutdown --exec-user=veynode --allow-files --files-user=veynode-files --files-root=/home/veynode-files/transfers > /run/agent-$1.log 2>&1 &
 agentpid=$!
}
wait_ready() {
 n=0
 while ! grep -q 'veynode guest agent protocol=' /run/agent-$1.log; do
  kill -0 "$agentpid"
  n=$((n+1)); test "$n" -lt 100
  sleep .05
 done
 cat /run/agent-$1.log
}
agent first
oldpid=$agentpid
wait_ready first
echo CGROUP_RECOVERY_FIRST_READY pid=$oldpid
n=0
while test ! -f /home/veynode/recovery-ready; do n=$((n+1));test "$n" -lt 300;sleep .1;done
oldgroup=$(find /sys/fs/cgroup/veynode-exec -mindepth 1 -maxdepth 1 -type d | head -1)
test -n "$oldgroup"
echo CGROUP_RECOVERY_OLD_GROUP=$oldgroup
cat "$oldgroup/cgroup.events"
kill -KILL "$oldpid"
set +e
wait "$oldpid"
killed=$?
set -e
test "$killed" = 137
echo CGROUP_RECOVERY_SIGKILL_EXIT=$killed
grep -q 'populated 1' "$oldgroup/cgroup.events"
echo CGROUP_RECOVERY_STALE_STILL_POPULATED
# EOF lets the old independent files worker close its directory lock.
sleep .2
agent second
newpid=$agentpid
wait_ready second
test ! -d "$oldgroup"
left=$(find /sys/fs/cgroup/veynode-exec -mindepth 1 -maxdepth 1 -type d | wc -l)
test "$left" = 0
echo CGROUP_RECOVERY_RESTART_READY pid=$newpid stale_groups=$left
printf complete > /home/veynode/recovery-complete
n=0
while test ! -f /home/veynode/recovery-finish; do n=$((n+1));test "$n" -lt 300;sleep .1;done
kill -TERM "$newpid"
wait "$newpid"
echo CGROUP_RECOVERY_ROOT_PASS
trap - EXIT
poweroff -f
'''
init=r'''#!/bin/sh
set -eu
trap 'echo CGROUP_RECOVERY_INIT_FAIL; poweroff -f' EXIT
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
modprobe virtio_blk
modprobe ext4
insmod /veynode/modules/vsock.ko
insmod /veynode/modules/vmw_vsock_virtio_transport_common.ko
insmod /veynode/modules/vmw_vsock_virtio_transport.ko
mkdir -p /newroot
mount /dev/vda /newroot
for fs in proc sys dev; do mount --bind /$fs /newroot/$fs; done
mount -t tmpfs tmpfs /newroot/run
cp /veynode/recovery-controller /newroot/run/recovery-controller
chmod 755 /newroot/run/recovery-controller
chroot /newroot /bin/sh /run/recovery-controller
'''
evidence={'scope':'Actual service SIGKILL and restart; RAM root harness, reused owned installed disk; no host/Guest privilege bypass through RPC','checks':[],'started_at_unix':time.time()}
with tempfile.TemporaryDirectory(prefix='vm-cgroup-recovery-') as temp:
 temp=Path(temp);initrd=temp/'initrd.gz';initrd.write_bytes((MEDIA/'veynode-initramfs.gz').read_bytes()+gzip.compress(fixture.entry('veynode/recovery-init',init.encode(),0o100755)+fixture.entry('veynode/recovery-controller',controller.encode(),0o100755)+fixture.entry('TRAILER!!!',b''),mtime=0))
 config.update(id='cgroup-recovery',initrd=str(initrd),command_line='console=hvc0 rdinit=/veynode/recovery-init',serial_log=str(OUT/'serial.log'))
 state=temp/'state';daemon=subprocess.Popen([str(ROOT/'target/debug/vm-control'),'daemon','--state-dir',str(state),'--backend',str(ROOT/'.local/bin/vm-apple')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
 clients=[]
 def local(method,payload):return json.loads(subprocess.check_output([str(ROOT/'target/debug/vm-control'),'call','--state-dir',str(state),method,json.dumps(payload)],text=True,timeout=40))
 def tool(client,name,**payload):return client.tool(name,{'id':config['id'],**payload})
 def execute(client,argv):return tool(client,'vm_exec',argv=argv,timeout_ms=2000)['result']
 try:
  for _ in range(100):
   if (state/'control.sock').exists():break
   time.sleep(.05)
  local('create',config);owner=MCP(state);clients.append(owner);tool(owner,'vm_start')
  for _ in range(150):
   try:info=tool(owner,'vm_guest_info')['result'];break
   except AssertionError:time.sleep(.1)
  else:raise TimeoutError('first real service unavailable')
  assert info['capabilities']['descendant_containment'] and info['file_policy']['uid']==1001,info
  evidence['checks'].append({'name':'first service real containment and files worker','evidence':info})
  script='setsid /bin/sh -c \'(sleep 5; printf escaped > "$2") </dev/null >/dev/null 2>&1 & printf ready > "$1"; exit 0\' detached "$1" "$2" & sleep 20 & wait'
  job=tool(owner,'vm_exec_start',argv=['/bin/sh','-c',script,'test','/home/veynode/recovery-ready','/home/veynode/recovery-marker'],timeout_ms=10000)['result']['job_id']
  for _ in range(150):
   text=(OUT/'serial.log').read_text(errors='replace')
   if 'CGROUP_RECOVERY_ROOT_FAIL' in text or 'CGROUP_RECOVERY_INIT_FAIL' in text:raise AssertionError('root controller failed; see serial')
   if 'CGROUP_RECOVERY_RESTART_READY' in text:break
   time.sleep(.1)
  else:raise TimeoutError('service restart not confirmed')
  assert 'CGROUP_RECOVERY_SIGKILL_EXIT=137' in text and 'CGROUP_RECOVERY_STALE_STILL_POPULATED' in text,text[-3000:]
  info2=tool(owner,'vm_guest_info')['result'];assert info2['capabilities']['descendant_containment'] and info2['file_policy']['uid']==1001,info2
  evidence['checks'].append({'name':'restarted real service and new files worker','evidence':info2})
  time.sleep(5.2)
  result=execute(owner,['/bin/sh','-ec','test ! -e /home/veynode/recovery-marker; test -e /home/veynode/recovery-complete; cat /proc/self/cgroup'])
  assert result['exit_code']==0 and '/veynode-exec/job-' in result['stdout'],result
  evidence['checks'].append({'name':'old detached descendant never writes marker; new atomic job works','evidence':result})
  # Root controller observes this test-owned sentinel, gracefully stops service,
  # then powers off this custom PID1 fixture (not installed init shutdown proof).
  assert execute(owner,['/bin/sh','-c','printf finish > /home/veynode/recovery-finish'])['exit_code']==0
  for _ in range(150):
   if local('status',{'id':config['id']})['state']=='stopped':break
   time.sleep(.1)
  else:raise TimeoutError('diagnostic fixture did not power off')
  text=(OUT/'serial.log').read_text(errors='replace');assert 'CGROUP_RECOVERY_ROOT_PASS' in text,text[-3000:]
  evidence['root_markers']=[l for l in text.splitlines() if 'CGROUP_RECOVERY_' in l]
  evidence['passed']=True
 except Exception as error:evidence.update(passed=False,error=str(error));raise
 finally:
  for client in clients:
   try:client.close()
   except Exception:pass
  try:
   if local('status',{'id':config['id']})['state']!='stopped':evidence['forced_failure_cleanup']=local('force_stop',{'id':config['id'],'confirm':config['id']})
  finally:
   daemon.terminate();daemon.wait(timeout=8);evidence['finished_at_unix']=time.time();(OUT/('result.json' if evidence.get('passed') else 'result.failed.json')).write_text(json.dumps(evidence,indent=2)+'\n')
print('Actual service SIGKILL / stale cgroup restart cleanup passed')
