#!/usr/bin/env python3
"""Host-only configuration/template tests. No image creation or VM boot."""
import importlib.util
from pathlib import Path
import subprocess
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('prepare_crash',ROOT/'scripts/prepare-qemu-guest-crash.py')
prepare=importlib.util.module_from_spec(spec);spec.loader.exec_module(prepare)

class Preparation(unittest.TestCase):
    def test_config_separates_maintenance_from_installed_exec_and_fault_nonce(self):
        source={'id':'old','backend':'qemu-hvf','guest':'linux','qemu_guest_exec':True,
                'command_line':'console=ttyAMA0 root=/dev/vda','kernel':'/kernel','initrd':'/boot-initrd','disk':'/old'}
        before=dict(source)
        maintenance,installed=prepare.configs(Path('/new'),source,Path('/new/disk'),Path('/new/ram'),'a'*64)
        self.assertEqual(source,before)
        self.assertNotIn('qemu_guest_exec',maintenance)
        self.assertNotIn('guest_crash_once',maintenance['command_line'])
        self.assertTrue(installed['qemu_guest_exec'])
        self.assertIn('veynode.guest_crash_once='+'a'*64,installed['command_line'])
        self.assertEqual(installed['initrd'],'/boot-initrd')
        self.assertEqual(maintenance['disk'],installed['disk'])

    def test_templates_parse_and_do_not_replace_production_service(self):
        folder=ROOT/'scripts/fixtures/guest-crash'
        for name in ('watcher.openrc','maintenance.sh'):
            subprocess.run(['/bin/sh','-n',str(folder/name)],check=True,timeout=2)
        text=(folder/'maintenance.sh').read_text()
        self.assertEqual(text.count('sha256sum -c /veynode/crash/preserved.sha256'),2)
        self.assertNotIn('cp /veynode/crash/agent',text)
        self.assertIn('mkdir -m 711 /newroot/var/lib/veynode-crash-once',text)
        self.assertIn('chown 1000:1000 /newroot/var/lib/veynode-crash-once/arm',text)
        self.assertIn('rc_crashed_start="NO"',text)

    def test_maintenance_script_refuses_host(self):
        r=subprocess.run(['/bin/sh',str(ROOT/'scripts/fixtures/guest-crash/maintenance.sh')],capture_output=True,timeout=2)
        self.assertNotEqual(r.returncode,0)
        self.assertNotIn(b'MAINTENANCE_PASS',r.stdout)

    def test_linux_arm_host_simulation_refused_before_trap_or_mount(self):
        script=ROOT/'scripts/fixtures/guest-crash/maintenance.sh'
        # Source in an ordinary non-PID1 shell, even when uname looks exactly
        # like the Guest. Stub all destructive commands so a regressed test
        # cannot mount filesystems or power off an actual Linux ARM CI host.
        simulated=r'''
uname() { if test "$1" = -s; then echo Linux; else echo aarch64; fi; }
mount() { echo UNEXPECTED_MOUNT; return 1; }
sync() { echo UNEXPECTED_SYNC; }
poweroff() { echo UNEXPECTED_POWEROFF; }
. "$1"
'''
        result=subprocess.run(['/bin/sh','-c',simulated,'host-guard',str(script)],capture_output=True,timeout=2)
        self.assertNotEqual(result.returncode,0)
        self.assertEqual(result.stdout,b'')
        self.assertNotIn(b'MAINTENANCE_FAIL',result.stderr)
        text=script.read_text()
        self.assertLess(text.index('test "$$" -eq 1'),text.index('trap failed EXIT'))
        self.assertLess(text.index('test -d /veynode/crash'),text.index('trap failed EXIT'))

if __name__=='__main__':unittest.main(verbosity=2)
