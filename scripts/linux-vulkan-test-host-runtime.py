#!/usr/bin/env python3
"""Private real-Venus lifecycle diagnostic; preparation is default, --run explicit.
No disk, no process scanning. Renderer awaits an authenticated private Unix ACK
before exec so the observer can register NOTE_EXIT without STOP/SIGCHLD. This timing
instrumentation is diagnostic only, never production behavior or a benchmark.
"""
import argparse
import ctypes
import gzip
import importlib.util
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import struct
import subprocess
import tempfile
import time
ROOT=Path(__file__).resolve().parents[1]
def module(name,path):
    s=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
B=module('vulkan_probe',ROOT/'scripts/linux-vulkan-probe.py')
F=module('initrd',ROOT/'scripts/build-linux-fixture.py')

WAIT_SOURCE = """#include <sys/wait.h>
#include <errno.h>
#include <libproc.h>
#include <sys/proc.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
int vn_peer_pid(int fd) {
    uid_t uid; gid_t gid; pid_t pid = 0; socklen_t size = sizeof(pid);
    if (getpeereid(fd, &uid, &gid) || uid != getuid() ||
        getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &pid, &size) || size != sizeof(pid)) return -1;
    return pid;
}
int vn_stopped(int pid) {
    struct proc_bsdinfo info = {0};
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) != sizeof(info)) return -1;
    return info.pbi_status == SSTOP;
}
int vn_parent_pid(int pid) {
    struct proc_bsdinfo info = {0};
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) != sizeof(info)) return -1;
    return info.pbi_ppid;
}
int vn_child_terminal(int pid) {
    siginfo_t info = {0};
    if (waitid(P_PID, (id_t)pid, &info, WEXITED | WNOHANG | WNOWAIT)) return -errno;
    return info.si_pid == pid && (info.si_code == CLD_EXITED || info.si_code == CLD_KILLED || info.si_code == CLD_DUMPED);
}
"""

def waiter(output):
    source=output/'wait-owner.c';source.write_text(WAIT_SOURCE)
    library=output/'wait-owner.dylib'
    subprocess.run(['clang','-Wall','-Wextra','-Werror','-dynamiclib',str(source),'-o',str(library)],check=True)
    loaded=ctypes.CDLL(str(library));fn=loaded.vn_child_terminal;fn.argtypes=[ctypes.c_int];fn.restype=ctypes.c_int
    parent=loaded.vn_parent_pid;parent.argtypes=[ctypes.c_int];parent.restype=ctypes.c_int
    stopped=loaded.vn_stopped;stopped.argtypes=[ctypes.c_int];stopped.restype=ctypes.c_int
    parent.stopped=stopped
    peer=loaded.vn_peer_pid;peer.argtypes=[ctypes.c_int];peer.restype=ctypes.c_int
    parent.peer=peer
    return fn,parent

def adopt_renderer(pid,ppid,pgid,owner,known,registered,kq,parent_pid):
    if len(known)>=64 or pid<=1 or pid in known or ppid not in {owner,*known} or parent_pid(pid)!=ppid:
        raise ValueError('renderer_parent_identity_mismatch')
    # Track the exact owned child before group validation; rejection closes its
    # waiting socket, and NOTE_EXIT observes that bounded failure too.
    kq.control([select.kevent(pid,filter=select.KQ_FILTER_PROC,flags=select.KQ_EV_ADD|select.KQ_EV_ONESHOT,fflags=select.KQ_NOTE_EXIT)],0,0)
    known[pid]={'pid':pid,'ppid':ppid,'pgid':pgid,'acknowledged':False};registered.add(pid)
    if pgid!=owner or os.getpgid(pid)!=owner:raise ValueError('renderer_owned_group_mismatch')

def listener(path):
    sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
    try:
        sock.bind(str(path));os.chmod(path,0o600);sock.listen(8);sock.setblocking(False)
        return sock
    except BaseException:
        sock.close();raise

def observe_connection(conn,owner,known,registered,kq,parent_pid):
    with conn:
        # One fixed-size message and one total deadline, never JSON/log parsing.
        peer=parent_pid.peer(conn.fileno())
        if peer<=1:raise ValueError('renderer_peer_identity_mismatch')
        end=time.monotonic()+1;data=b''
        conn.setblocking(False)
        while len(data)<12:
            left=end-time.monotonic()
            if left<=0 or not select.select([conn],[],[],left)[0]:raise TimeoutError('renderer_tuple_deadline')
            chunk=conn.recv(12-len(data))
            if not chunk:raise ValueError('renderer_tuple_eof')
            data+=chunk
        pid,ppid,pgid=struct.unpack('!III',data)
        if pid!=peer:raise ValueError('renderer_peer_identity_mismatch')
        adopt_renderer(pid,ppid,pgid,owner,known,registered,kq,parent_pid)
        if time.monotonic()>=end:raise TimeoutError('renderer_ack_deadline')
        # A single byte on a fresh socket cannot create an unbounded send queue.
        if conn.send(b'1')!=1:raise ValueError('renderer_ack_short')
        known[pid]['acknowledged']=True

def compile_observer(output,server,deadline_ms=5000):
    wrapper=output/'renderer-observer';source=output/'renderer-observer.c'
    source.write_bytes((ROOT/'scripts/linux-vulkan-render-observer.c').read_bytes())
    subprocess.run(['clang','-Wall','-Wextra','-Werror','-DVN_SERVER='+json.dumps(str(server)),
                    '-DVN_DEADLINE_MS='+str(deadline_ms),str(source),'-o',str(wrapper)],check=True)
    subprocess.run(['codesign','-s','-',str(wrapper)],check=True,capture_output=True)
    return wrapper

def drain_exits(kq,registered,events,seconds):
    until=time.monotonic()+seconds
    while registered and time.monotonic()<until:
        for event in kq.control(None,64,min(.05,max(0,until-time.monotonic()))):
            if event.fflags & select.KQ_NOTE_EXIT:
                registered.discard(event.ident);events.append(int(event.ident))

def kill_unacknowledged_outliers(known,registered,parent_pid):
    killed=[]
    for pid in list(registered):
        entry=known[pid]
        if not entry['acknowledged'] and parent_pid(pid)==entry['ppid']:
            try:os.kill(pid,signal.SIGKILL);killed.append(pid)
            except ProcessLookupError:pass
    return killed

def prepare(manifest,output,mode):
    m=json.loads(manifest.read_text())
    for key in ['binary','host_library','loader']:
        if B.digest(Path(m[key]))!=m[key+'_sha256']:raise ValueError('runtime hash mismatch')
    output.mkdir(mode=0o700,parents=True,exist_ok=False)
    wrapper=compile_observer(output,m['binary'])
    observed={**m,'binary':str(wrapper),'binary_sha256':B.digest(wrapper)}
    (output/'observer-manifest.json').write_text(json.dumps(observed,indent=2)+'\n')
    cmd,env=B.prepare(output/'guest',output/'observer-manifest.json',4096,True)
    cmd[0]=m['loader'];env['VEYNODE_VIRGL_LIBRARY']=m['host_library']
    if mode=='group-stop':
        data=(ROOT/'scripts/linux-vulkan-triangle.c').read_text()
        anchor='  OK(vkDeviceWaitIdle(dev));\n  vkDestroyFence'
        if data.count(anchor)!=1:raise ValueError('triangle hold anchor')
        data='#include <unistd.h>\n'+data.replace(anchor,'  OK(vkDeviceWaitIdle(dev));\n  printf("VK_LIFECYCLE_HOLD\\n"); fflush(stdout); sleep(20);\n  vkDestroyFence')
        (output/'triangle-hold.c').write_text(data)
        with (output/'guest/initramfs.gz').open('ab') as f:
            f.write(gzip.compress(F.entry('veynode/triangle.c',data.encode(),0o100644)+F.entry('TRAILER!!!',b''),mtime=0))
    plan={'schema':1,'mode':mode,'command':cmd,'environment':env,'runtime_manifest_sha256':B.digest(manifest),
          'initrd_sha256':B.digest(output/'guest/initramfs.gz'),'observer_sha256':B.digest(wrapper),
          'observer_source_sha256':B.digest(output/'renderer-observer.c'),
          'instrumentation':'private same-UID/peer-PID Unix ACK after exact parent/group and NOTE_EXIT; no STOP/CONT; then original server exec; fault triangle holds context20s after pixel checks',
          'prepared_only':True}
    (output/'plan.json').write_text(json.dumps(plan,indent=2)+'\n')
    return plan

def run(out,plan):
    report={'schema':1,'mode':plan['mode'],'passed':False,'renderer_owners':[], 'renderer_exits':[],
            'observer_protocol':'private-unix-ack-v1','descendants_reaped_by_harness':False,'owned_qemu_reaped':False,'group_stop_sent':False}
    proc=None;kq=select.kqueue();known={};registered=set();observer=None;terminal_state,parent_pid=waiter(out)
    try:
        with tempfile.TemporaryDirectory(prefix='vn-hostlife-') as tmp,(out/'serial.log').open('xb') as log:
            cmd=[x.replace('@SOCKET@',tmp+'/spice.sock') for x in plan['command']]
            env=os.environ.copy()
            for name in list(env):
                if name.startswith(('VK_','VIRGL_','RENDER_SERVER_')) or name=='APP_SANDBOX_GROUP_ID':env.pop(name)
            env.update(plan['environment'])
            observer=listener(Path(tmp)/'observer.sock')
            env['VN_RENDER_OBSERVER_SOCKET']=tmp+'/observer.sock'
            env['VN_RENDER_OBSERVER_PID']=str(os.getpid())
            proc=subprocess.Popen(cmd,env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log,start_new_session=True)
            report['qemu_pid']=proc.pid;deadline=time.monotonic()+180
            while True:
                if time.monotonic()>deadline:raise TimeoutError('run_deadline')
                data=(out/'serial.log').read_bytes()
                if len(data)>8*1024*1024:raise ValueError('log_bound')
                # At most eight accepts per loop; each read is bounded to one second.
                for _ in range(8):
                    try:conn,_=observer.accept()
                    except BlockingIOError:break
                    observe_connection(conn,proc.pid,known,registered,kq,parent_pid)
                    report['renderer_owners']=list(known.values())
                for event in kq.control(None,64,0):
                    if event.fflags & select.KQ_NOTE_EXIT:registered.discard(event.ident);report['renderer_exits'].append(int(event.ident))
                if plan['mode']=='group-stop' and b'VK_LIFECYCLE_HOLD' in data and not report['group_stop_sent']:
                    cases=re.findall(rb'VK_PIXEL_CASE frame=(\d+) tested=(\d+) mismatches=(\d+)',data)
                    if len(cases)!=3 or any(n!=b'3522' or bad!=b'0' for _,n,bad in cases):raise ValueError('pixel_hold_precondition')
                    if not any(v['ppid']!=proc.pid and pid in registered for pid,v in known.items()):raise ValueError('no_live_context_worker')
                    # Exact process group created by start_new_session, QEMU still unreaped.
                    os.killpg(proc.pid,signal.SIGKILL);report['group_stop_sent']=True
                terminal=terminal_state(proc.pid)
                if terminal<0:raise OSError(-terminal,'owned child waitid failed')
                if terminal and not registered:break
                time.sleep(.01)
            report.update(exit_code=proc.wait(timeout=3),owned_qemu_reaped=True)
        text=(out/'serial.log').read_text(errors='replace')
        if len(known)<2:raise ValueError('missing_server_context_observation')
        if plan['mode']=='normal':
            report['pixel_result']=B.classify_pixels(text,B.classify(text,report['exit_code']))
            if not report['pixel_result']['raster_pixel_oracle_passed']:raise ValueError('normal_pixel_or_exit_failure')
        elif report['exit_code']!=-signal.SIGKILL or not report['group_stop_sent']:raise ValueError('fault_not_exercised')
        # NOTE_EXIT proves observed process termination, not waitpid ownership of descendants.
        report['passed']=True
    except Exception as e:report['error']=type(e).__name__+':'+str(e)[:160]
    finally:
        report['renderer_owners']=list(known.values())
        try:report['unacknowledged_outliers_killed']=kill_unacknowledged_outliers(known,registered,parent_pid)
        except Exception as e:report['fallback_error']=type(e).__name__;report['passed']=False
        if proc is not None and proc.returncode is None:
            try:os.killpg(proc.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            report['fallback_group_kill']=True
            drain_exits(kq,registered,report['renderer_exits'],5)
            try:report['exit_code']=proc.wait(timeout=5);report['owned_qemu_reaped']=True
            except subprocess.TimeoutExpired:report['fallback_error']='owned_qemu_reap_timeout';report['passed']=False
        drain_exits(kq,registered,report['renderer_exits'],2)
        report['renderer_exit_unconfirmed']=sorted(registered);kq.close()
        if observer is not None:observer.close()
        if (out/'serial.log').exists():report['serial_sha256']=B.digest(out/'serial.log')
        (out/'result.json').write_text(json.dumps(report,indent=2)+'\n')
    return report

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--runtime-manifest',type=Path,required=True);p.add_argument('--output-dir',type=Path,required=True);p.add_argument('--mode',choices=['normal','group-stop'],required=True);p.add_argument('--run',action='store_true');a=p.parse_args();os.umask(0o077)
    out=a.output_dir.absolute();plan=prepare(a.runtime_manifest.resolve(),out,a.mode)
    if not a.run:print(json.dumps({'prepared':True,'vm_run':False}));return 0
    result=run(out,plan);print(json.dumps(result));return 0 if result['passed'] else 1
if __name__=='__main__':raise SystemExit(main())
