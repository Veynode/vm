#!/usr/bin/env python3
"""Real QEMU/HVF -> daemon -> MCP lifecycle/screenshot, plus VZ crash isolation.
Creates a fresh RAM-only Linux fixture; never attaches existing disks.
Requires pinned Alpine/QEMU diagnostic prerequisites; no software fallback.
"""
import argparse, base64, concurrent.futures, gzip, hashlib, importlib.util, json, os
from pathlib import Path
import select, signal, socket, stat, subprocess, sys, tarfile, tempfile, time
ROOT=Path(__file__).resolve().parents[1]
BASE=ROOT/'.local/runtime/utm'; MEDIA=ROOT/'.local/media/alpine-3.24.2'
def load(path,name):
    spec=importlib.util.spec_from_file_location(name,path); module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); return module
def fixture(directory,static=False,input_oracle=None,input_oracle_seconds=180,init_transform=None):
    if type(input_oracle_seconds) is not int or not 30 <= input_oracle_seconds <= 3600: raise ValueError("invalid input oracle lifetime")
    desktop=load(ROOT/'scripts/qemu-linux-desktop-probe.py','qemu_desktop')
    archive=BASE/'alpine-minirootfs-3.23.6-aarch64.tar.gz'
    assert hashlib.sha256(archive.read_bytes()).hexdigest()==desktop.ROOTFS_SHA
    cpio=load(ROOT/'scripts/build-linux-fixture.py','qemu_cpio'); entries=[]
    with tarfile.open(archive) as source:
        for member in source.getmembers():
            name=member.name.removeprefix('./')
            if name in ('','.'): continue
            assert not name.startswith('/') and '..' not in Path(name).parts
            if member.isdir(): mode,data=stat.S_IFDIR|member.mode,b''
            elif member.issym(): mode,data=stat.S_IFLNK|member.mode,member.linkname.encode()
            elif member.isfile(): mode,data=stat.S_IFREG|member.mode,source.extractfile(member).read()
            else: raise AssertionError('Unexpected archive object')
            entries.append(cpio.entry(name,data,mode))
    version='6.18.52-0-virt'; modloop=MEDIA/'modloop-virt'
    deps=dict(line.split(':',1) for line in subprocess.check_output(['unsquashfs','-cat',str(modloop),f'modules/{version}/modules.dep'],text=True).splitlines()); modules=set()
    def include(name):
        if name in modules:return
        modules.add(name)
        for dep in deps[name].split(): include(dep)
    for name in ['kernel/drivers/acpi/button.ko','kernel/drivers/input/evdev.ko','kernel/drivers/virtio/virtio_input.ko']: include(name)
    directories=set()
    for name in sorted(modules):
        target=Path(f'usr/lib/modules/{version}/{name}')
        for parent in reversed(target.parents):
            if str(parent)!='.' and parent not in directories:
                directories.add(parent);entries.append(cpio.entry(str(parent),b'',0o40755))
        entries.append(cpio.entry(str(target),subprocess.check_output(['unsquashfs','-cat',str(modloop),f'modules/{version}/{name}'])))
    init=desktop.INIT.replace('modprobe virtio_gpu','modprobe virtio_gpu\nmodprobe button\nmodprobe evdev\nmodprobe virtio_input')
    init=init.replace('Option "AutoAddDevices" "false"','Option "AutoAddDevices" "true"')
    # Deliberately retain stock renderer defects; this test proves lifecycle/capture, not pixel correctness.
    init=init.replace('echo VEYNODE_DESKTOP_READY\nsleep 300\ntrap - EXIT\npoweroff -f','''cat /proc/bus/input/devices
acpid -d &
sleep 1
echo VEYNODE_DESKTOP_READY
trap - EXIT
exec /sbin/init''')
    if static:
        init=init.replace('while true; do date; sleep 1; done','printf static-console-fixture; sleep 600')
        init=init.replace('kill -0 "$gears"','kill -0 "$gears"\nkill "$gears"\nwait "$gears" || true\nsleep 2')
    if input_oracle:
        source=Path(input_oracle).read_bytes()
        init=init.replace('apk add --no-cache --no-progress','mount -t tmpfs -o remount,size=1100M rootfs /\napk add --no-cache --no-progress').replace('font-dejavu eudev','font-dejavu eudev build-base libx11-dev linux-headers')
        init=init.replace('twm &',f"base64 -d > /veynode/input-oracle.c <<'B64ORACLE'\n{base64.b64encode(source).decode()}\nB64ORACLE\ncc -Wall -Wextra -Werror -O2 /veynode/input-oracle.c -o /veynode/input-oracle -lX11\n"+'twm &')
        init=init.replace('echo VEYNODE_DESKTOP_READY',f'/veynode/input-oracle {input_oracle_seconds} &\nsleep 1\necho VEYNODE_DESKTOP_READY')
    if init_transform is not None:
        init=init_transform(init)
        if not isinstance(init,str) or not init.startswith('#!/bin/sh\n'):
            raise ValueError('invalid fixture init transformation')
        subprocess.run(['/bin/sh','-n'],input=init,text=True,check=True)
    power='#!/bin/sh\necho VEYNODE_ACPI_POWER_BUTTON >/dev/console\n/sbin/poweroff\n'
    shutdown='#!/bin/sh\necho VEYNODE_INIT_NORMAL_SHUTDOWN >/dev/console\nsync\n'
    for name,data,mode in [('veynode/gpu-init',init.encode(),0o100755),('veynode/dhcp',desktop.DHCP.encode(),0o100755),('veynode/shutdown',shutdown.encode(),0o100755),('etc/inittab',b'::sysinit:/bin/true\n::shutdown:/veynode/shutdown\n',0o100644),('etc/acpi',b'',0o40755),('etc/acpi/PWRF',b'',0o40755),('etc/acpi/PWRF/00000080',power.encode(),0o100755)]: entries.append(cpio.entry(name,data,mode))
    entries.append(cpio.entry('TRAILER!!!',b'')); output=directory/'qemu-initrd.gz'
    output.write_bytes((MEDIA/'veynode-initramfs.gz').read_bytes()+gzip.compress(b''.join(entries),mtime=0))
    return output,{'rootfs_sha256':desktop.ROOTFS_SHA,'initrd_sha256':hashlib.sha256(output.read_bytes()).hexdigest(),'modules':sorted(modules),'normal_shutdown':'UEFI ACPI button -> evdev -> BusyBox acpid -> poweroff (no -f) -> PID1 shutdown action'}
class MCP:
    def __init__(self,binary,state,lifecycle=False,input=False,execute=False):
        self.p=subprocess.Popen([str(binary),'mcp','--state-dir',str(state)]+(['--allow-lifecycle'] if lifecycle else [])+(['--allow-input'] if input else [])+(['--allow-execute'] if execute else []),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1);self.number=0
        self.rpc('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'real-qemu-test','version':'1'}})
        self.p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n');self.p.stdin.flush()
    def rpc(self,method,params):
        self.number+=1;self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.number,'method':method,'params':params})+'\n');self.p.stdin.flush()
        if not select.select([self.p.stdout],[],[],40)[0]:raise TimeoutError('MCP reply timeout')
        reply=json.loads(self.p.stdout.readline());assert reply['id']==self.number and 'error' not in reply,reply;return reply['result']
    def tool(self,name,args):
        return self.rpc('tools/call',{'name':name,'arguments':args})
    def close(self):
        if self.p.poll() is None:self.p.stdin.close();self.p.wait(timeout=10)
def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=Path,required=True);parser.add_argument('--persistent-console',action='store_true');parser.add_argument('--static-desktop',action='store_true');parser.add_argument('--extra-console-consumer',type=Path);args=parser.parse_args();output=args.output.resolve();output.parent.mkdir(parents=True,exist_ok=True)
    if output.exists():raise SystemExit('Refusing evidence overwrite')
    result={'passed':False,'checks':[],'started_at_unix':time.time(),'scope':'Real Linux ARM RAM fixture. Stock graphics defects retained. No Windows, disk storage, native window, performance, or production release claim.'}
    def record(name,evidence):result['checks'].append({'name':name,'evidence':evidence});print(name,flush=True)
    binary=ROOT/'target/debug/vm-control';worker=ROOT/'target/debug/vm-qemu';apple=ROOT/'.local/bin/vm-apple'
    runtime=['--qemu-worker',str(worker),'--qemu-launcher',str(ROOT/'.local/spice-reconnect-build/runtime/probe-loader'),'--qemu-spice-library',str(ROOT/'.local/spice-reconnect-build/runtime/libspice-server-fixed.dylib'),'--qemu-library',str(BASE/'UTM.app/Contents/Frameworks/qemu-aarch64-softmmu.framework/Versions/A/qemu-aarch64-softmmu'),'--qemu-resources',str(BASE/'UTM.app/Contents/Resources/qemu'),'--qemu-screenshot-helper',str(ROOT/'.local/bin/spice-screenshot')]
    if (args.static_desktop or args.extra_console_consumer) and not args.persistent_console:parser.error('static/extra consumer requires --persistent-console')
    if args.persistent_console:
        runtime=runtime[:-2]+['--qemu-console-broker',str(ROOT/'target/debug/vm-console'),'--qemu-console-helper',str(ROOT/'.local/bin/spice-console')]
    result['persistent_console']=args.persistent_console;result['static_desktop']=args.static_desktop
    result['binary_sha256']={str(path.relative_to(ROOT)):hashlib.sha256(path.read_bytes()).hexdigest() for path in [binary,worker,Path(runtime[runtime.index('--qemu-launcher')+1]),Path(runtime[runtime.index('--qemu-spice-library')+1]),ROOT/('target/debug/vm-console' if args.persistent_console else '.local/bin/spice-screenshot'),*([ROOT/'.local/bin/spice-console'] if args.persistent_console else [])]}
    result['test_source_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix='vq-',dir='/tmp') as temp:
        directory=Path(temp);state=directory/'state';initrd,meta=fixture(directory,args.static_desktop);result['fixture']=meta
        daemon=subprocess.Popen([str(binary),'daemon','--state-dir',str(state),'--backend',str(apple)]+runtime,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE);clients=[];active=[]
        def call(method,payload):
            out=subprocess.run([str(binary),'call','--state-dir',str(state),method,json.dumps(payload)],capture_output=True,text=True,timeout=40)
            if out.returncode:raise RuntimeError(out.stderr)
            return json.loads(out.stdout)
        def wait_state(vm,expected,seconds=20):
            end=time.monotonic()+seconds
            while time.monotonic()<end:
                value=call('status',{'id':vm})
                if value['state'] in expected:return value
                time.sleep(.1)
            raise TimeoutError(f'{vm} state {value}')
        def check_reply(value):assert value.get('isError') is False,value;return json.loads(value['content'][0]['text'])
        try:
            for _ in range(100):
                if (state/'control.sock').exists():break
                if daemon.poll() is not None:raise RuntimeError(daemon.stderr.read().decode())
                time.sleep(.05)
            mcp=MCP(binary,state,True);clients.append(mcp)
            config={'id':'qemu-console','backend':'qemu-hvf','guest':'linux','cpus':2,'memory_mib':1536,'kernel':str(MEDIA/'Image'),'initrd':str(initrd),'command_line':'console=ttyAMA0 rdinit=/veynode/gpu-init','serial_log':str(state/'qemu.serial.log')}
            check_reply(mcp.tool('vm_create',config));active.append(config['id'])
            check_reply(mcp.tool('vm_start',{'id':config['id']}));running=wait_state(config['id'],['running']);assert running['runtime']['guest_ready'] is None
            serial=Path(running['runtime']['serial_log']);end=time.monotonic()+180
            while time.monotonic()<end:
                log=serial.read_text(errors='replace') if serial.exists() else ''
                if 'VEYNODE_DESKTOP_FAIL' in log:raise RuntimeError(log[-4000:])
                if 'VEYNODE_DESKTOP_READY' in log:break
                time.sleep(.2)
            else:raise TimeoutError(log[-4000:])
            assert 'ACPI 2.0=' in log and 'Accelerated: yes' in log and 'Device: virgl' in log
            record('mcp_start_real_linux_qmp_spice',{'state':running,'serial_verified_virgl':True,'guest_ready_only_test_serial_marker':True})
            shot=mcp.tool('vm_screenshot',{'id':config['id']});assert not shot.get('isError'),shot
            assert shot['content'][0]['type']=='image';png=base64.b64decode(shot['content'][0]['data'],validate=True);assert png.startswith(b'\x89PNG\r\n\x1a\n')
            metadata=json.loads(shot['content'][1]['text']);assert metadata['byte_length']==len(png) and 'internal_scanout_id' not in metadata
            output.with_suffix('.png').write_bytes(png);record('mcp_observation_png',metadata)
            # Concurrent real capture calls are serialized by a single worker slot, not global blocking.
            with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
                attempts=list(pool.map(lambda _:subprocess.run([str(binary),'call','--state-dir',str(state),'screenshot',json.dumps({'id':config['id']})],capture_output=True,text=True,timeout=25),range(6)))
            result['capture_attempts']=[{'returncode':x.returncode,'error':x.stderr} for x in attempts]
            assert any('screenshot busy' in x.stderr for x in attempts)
            assert any(x.returncode==0 for x in attempts)
            record('bounded_concurrent_capture',{'successful':sum(x.returncode==0 for x in attempts),'busy':sum('screenshot busy' in x.stderr for x in attempts)})
            mcp.close();assert call('status',{'id':config['id']})['state']=='running';record('mcp_disconnect_keeps_vm_running',True)
            mcp=MCP(binary,state,True);clients.append(mcp)
            if args.static_desktop:
                time.sleep(3)
                second=mcp.tool('vm_screenshot',{'id':config['id']});assert not second['isError'],second
                later=json.loads(second['content'][1]['text'])
                assert later['source_generation']==metadata['source_generation'] and later['source_sequence']==metadata['source_sequence'],(metadata,later)
                assert later['captured_monotonic_us']==metadata['captured_monotonic_us']
                assert later['frame_id']!=metadata['frame_id']
                assert base64.b64decode(second['content'][0]['data'],validate=True)==png
                record('new_mcp_client_gets_unchanged_static_frame',{'first':metadata,'later':later,'same_png_sha256':hashlib.sha256(png).hexdigest(),'wait_seconds':3})
            if args.extra_console_consumer:
                prefix=output.with_suffix('.console');log_path=output.with_suffix('.consumer.log')
                with log_path.open('xb') as consumer_log:
                    consumer=subprocess.Popen([str(args.extra_console_consumer.resolve()),running['runtime']['console_socket'],str(prefix)],stdout=consumer_log,stderr=subprocess.STDOUT,start_new_session=True)
                    try:consumer.wait(timeout=180)
                    except subprocess.TimeoutExpired:
                        os.killpg(consumer.pid,signal.SIGTERM)
                        try:consumer.wait(timeout=5)
                        except subprocess.TimeoutExpired:os.killpg(consumer.pid,signal.SIGKILL);consumer.wait(timeout=5)
                        raise
                assert consumer.returncode==0, f'console consumer failed; see {log_path}'
                assert call('status',{'id':config['id']})['state']=='running'
                after=mcp.tool('vm_screenshot',{'id':config['id']});assert not after['isError'],after
                after_metadata=json.loads(after['content'][1]['text'])
                if args.static_desktop:
                    assert after_metadata['source_sequence']==metadata['source_sequence'] and base64.b64decode(after['content'][0]['data'],validate=True)==png
                record('extra_console_consumer_closed_without_losing_vm_or_capture',{'log':str(log_path),'after_screenshot':after_metadata})
            error=mcp.tool('vm_guest_info',{'id':config['id']});assert error['isError'] and 'guest integration unavailable' in error['content'][0]['text'];record('absent_guest_service_is_explicit_unavailable',True)
            ack=check_reply(mcp.tool('vm_shutdown',{'id':config['id']}));assert ack['requested'] and not ack['completed'];stopped=wait_state(config['id'],['stopped'],20)
            log=serial.read_text(errors='replace');assert 'VEYNODE_ACPI_POWER_BUTTON' in log and 'VEYNODE_INIT_NORMAL_SHUTDOWN' in log
            output.with_suffix('.serial.log').write_text(log);record('real_normal_shutdown_via_acpi_and_guest_init',{'ack':ack,'state':stopped,'button_marker':True,'init_shutdown_marker':True})
            vz={'id':'vz-isolation','guest':'linux','cpus':1,'memory_mib':512,'kernel':str(MEDIA/'Image'),'initrd':str(MEDIA/'veynode-initramfs.gz'),'command_line':'console=hvc0 rdinit=/veynode/init'}
            call('create',vz);active.append(vz['id']);call('start',{'id':vz['id']});wait_state(vz['id'],['running'])
            call('start',{'id':config['id']});again=wait_state(config['id'],['running']);qemu_pid=again['runtime']['backend_pid'];os.kill(again['pid'],signal.SIGKILL)
            crashed=wait_state(config['id'],['crashed']);time.sleep(.2)
            # A killed backend child may transiently be an init-owned zombie; it must not execute.
            process=subprocess.run(['ps','-p',str(qemu_pid),'-o','stat='],capture_output=True,text=True).stdout.strip();assert not process or process.startswith('Z'),process
            assert call('status',{'id':vz['id']})['state']=='running';record('worker_crash_reaps_qemu_and_vz_survives',{'state':crashed['state'],'qemu_process_state':process or 'absent','vz_state':'running'})
            call('start',{'id':config['id']});again=wait_state(config['id'],['running']);os.kill(again['pid'],signal.SIGTERM);wait_state(config['id'],['stopped']);record('worker_sigterm_cleanup',True)
            call('start',{'id':config['id']});again=wait_state(config['id'],['running']);qemu_pid=again['runtime']['backend_pid'];worker_pid=again['pid'];daemon.kill();daemon.wait(timeout=10)
            deadline=time.monotonic()+10
            while time.monotonic()<deadline:
                status=subprocess.run(['ps','-p',f'{qemu_pid},{worker_pid}','-o','stat='],capture_output=True,text=True).stdout.split()
                if all(x.startswith('Z') for x in status):break
                time.sleep(.1)
            else:raise AssertionError(f'daemon death left worker or QEMU alive: {status}')
            record('daemon_death_stdin_eof_cleans_worker_qemu',True)
            result['passed']=True
        except Exception as error:
            result['error']=repr(error)
            if daemon.poll() is None:
                states={}
                for vm in active:
                    try:states[vm]=call('status',{'id':vm})
                    except Exception as status_error:states[vm]={'status_error':str(status_error)}
                result['states_at_failure']=states
            raise
        finally:
            for client in clients:
                try:client.close()
                except Exception:pass
            if daemon.poll() is None:
                for vm in active:
                    try:call('force_stop',{'id':vm,'confirm':vm})
                    except Exception:pass
                daemon.terminate();daemon.wait(timeout=10)
            # Preserve all actual logs even on failures; no evidence assertion removed.
            for path in state.glob('*.log'):
                (output.parent/(output.stem+'.'+path.name)).write_bytes(path.read_bytes())
            result['finished_at_unix']=time.time();output.write_text(json.dumps(result,indent=2)+'\n')
    return 0
if __name__=='__main__':raise SystemExit(main())
