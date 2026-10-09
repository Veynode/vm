#!/usr/bin/env python3
"""Host component tests only; do not claim installed Guest acceptance."""
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('prepare', ROOT / 'scripts/prepare-installed-qemu-exec.py')
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class Preparation(unittest.TestCase):
    def test_bounded_input_rejects_links_fifo_and_oversize(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            regular = root / 'data'
            regular.write_bytes(b'abc')
            self.assertEqual(prepare.read_regular(regular, 3, hashlib.sha256(b'abc').hexdigest()), b'abc')
            with self.assertRaises(ValueError):
                prepare.read_regular(regular, 2)
            with self.assertRaises(ValueError):
                prepare.read_regular(regular, 3, '0' * 64)
            (root / 'link').symlink_to(regular)
            with self.assertRaises(OSError):
                prepare.read_regular(root / 'link', 3)
            os.mkfifo(root / 'fifo')
            with self.assertRaises(ValueError):
                prepare.read_regular(root / 'fifo', 3)

    def test_agent_requires_exact_arm64_pin(self):
        data = bytearray(64)
        data[:6] = b'\x7fELF\x02\x01'
        data[18:20] = (183).to_bytes(2, 'little')
        prepare.validate_agent(data, hashlib.sha256(data).hexdigest())
        for altered in [data[:18] + b'\x3e\0' + data[20:], b'not an ELF']:
            with self.subTest(altered=altered[:20]), self.assertRaises(ValueError):
                prepare.validate_agent(altered, hashlib.sha256(altered).hexdigest())
        with self.assertRaises(ValueError):
            prepare.validate_agent(data, '0' * 64)

    def test_installed_exec_cannot_leak_into_ram_maintenance(self):
        maintenance, installed = prepare.configs(Path('/fixture'), Path('/fixture/disk.raw'), Path('/kernel'), Path('/boot-initrd'), Path('/maintenance-initrd'))
        self.assertNotIn('qemu_guest_exec', maintenance)
        self.assertIs(installed['qemu_guest_exec'], True)
        self.assertEqual(installed['disk'], maintenance['disk'])
        self.assertIn('root=/dev/vda rootfstype=ext4', installed['command_line'])
        self.assertNotIn('rdinit=', installed['command_line'])
        self.assertIn('veynode.exec_fixture_maintenance=1', maintenance['command_line'])
        self.assertFalse(maintenance['window'])
        self.assertEqual((installed['width'], installed['height']), (2560, 1440))

    def test_cgroup_real_shell_gate_rejects_wrong_mount_and_owner(self):
        source = (ROOT / 'scripts/prepare-installed-qemu-exec-cgroup.sh').read_text()
        for kind, uid, controllers, accepted in [('cgroup2', '0', True, True), ('cgroup', '0', True, False),
                                                ('tmpfs', '0', True, False), ('cgroup2', '1000', True, False),
                                                ('cgroup2', '0', False, False)]:
            with self.subTest(kind=kind, uid=uid, controllers=controllers), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                cgroup = directory / 'cgroup'
                cgroup.mkdir()
                if controllers:
                    (cgroup / 'cgroup.controllers').write_text('cpu memory\n')
                mounts = directory / 'mounts'
                mounts.write_text(f'cgroup2 {cgroup} {kind} rw 0 0\n')
                script = source.replace('/sys/fs/cgroup', str(cgroup)).replace('/proc/mounts', str(mounts))
                # Only metadata query substitutions; the production awk/type gate is executed.
                functions = f'id() {{ echo 0; }}\nstat() {{ echo {uid}; }}\nmount() {{ echo UNEXPECTED_MOUNT; return 97; }}\n'
                result = subprocess.run(['/bin/sh'], input=functions + script, text=True, capture_output=True, timeout=2)
                self.assertEqual(result.returncode == 0, accepted, result.stderr)
                self.assertNotIn('UNEXPECTED_MOUNT', result.stdout)
                self.assertEqual('VEYNODE_EXEC_CGROUP2_MOUNT_VERIFIED' in result.stdout, accepted)

    def test_shells_parse_service_permissions_and_no_host_execution(self):
        for suffix in ['maintenance.sh', 'cgroup.sh', 'agent.openrc']:
            subprocess.run(['/bin/sh', '-n', str(ROOT / ('scripts/prepare-installed-qemu-exec-' + suffix))], check=True)
        service = (ROOT / 'scripts/prepare-installed-qemu-exec-agent.openrc').read_text()
        self.assertIn('command_args="--virtio-serial-exec --allow-execute --exec-user=veynode"', service)
        for forbidden in ['--allow-files', '--allow-shutdown', '--legacy-', 'respawn', 'supervise-daemon']:
            self.assertNotIn(forbidden, service)
        if os.uname().sysname != 'Linux':
            result = subprocess.run(['/bin/sh', str(ROOT / 'scripts/prepare-installed-qemu-exec-maintenance.sh')], capture_output=True, timeout=2)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn(b'VEYNODE_EXEC_MAINTENANCE_PASS', result.stdout)
            self.assertEqual(result.stderr, b'')

    def test_account_collision_guard_executes_before_write_mount(self):
        source = (ROOT / 'scripts/prepare-installed-qemu-exec-maintenance.sh').read_text()
        start = source.index('awk -F:')
        block = source[start:source.index('umount /newroot', start)]
        for passwd, group, home, accepted in [('root:x:0:0::/:/bin/sh\n', 'root:x:0:\n', False, True),
                                            ('other:x:1000:1000::/:/bin/sh\n', 'root:x:0:\n', False, False),
                                            ('veynode:x:2000:2000::/:/bin/sh\n', 'root:x:0:\n', False, False),
                                            ('root:x:0:0::/:/bin/sh\n', 'other:x:1000:\n', False, False),
                                            ('root:x:0:0::/:/bin/sh\n', 'root:x:0:\n', True, False)]:
            with self.subTest(passwd=passwd, group=group, home=home), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / 'etc').mkdir()
                (root / 'home').mkdir()
                (root / 'etc/passwd').write_text(passwd)
                (root / 'etc/group').write_text(group)
                if home:
                    (root / 'home/veynode').mkdir()
                result = subprocess.run(['/bin/sh'], input='set -eu\n' + block + '\necho ACCOUNT_CLEAR\n', cwd=root, text=True, capture_output=True, timeout=2)
                self.assertEqual(result.returncode == 0, accepted, result.stderr)
                self.assertEqual('ACCOUNT_CLEAR' in result.stdout, accepted)


    def test_real_unmount_blocks_leave_target_working_directory(self):
        source = (ROOT / 'scripts/prepare-installed-qemu-exec-maintenance.sh').read_text()
        chunks = []
        first = source.index('cd /newroot\n')
        chunks.append((source[first:source.index('umount /newroot', first) + len('umount /newroot')], False))
        final = source.index('cd /newroot\n', first + 1)
        chunks.append((source[final:source.index('umount /newroot', final) + len('umount /newroot')], True))
        for block, created in chunks:
            with self.subTest(created=created), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / 'etc').mkdir()
                (root / 'home').mkdir()
                (root / 'etc/passwd').write_text('veynode:x:1000:1000::/home/veynode:/bin/sh\n' if created else 'root:x:0:0::/:/bin/sh\n')
                (root / 'etc/group').write_text('root:x:0:\n')
                (root / 'etc/shadow').write_text('veynode:!:::::::\n')
                # A real shell CWD would keep ext4 busy even after all writes succeeded.
                functions = 'sha256sum() { return 0; }\nsync() { return 0; }\numount() { [ "$PWD" != "$1" ] || return 98; echo UNMOUNT_WITHOUT_OWN_CWD; }\n'
                result = subprocess.run(['/bin/sh'], input='set -eu\n' + functions + block.replace('/newroot', temporary), text=True, capture_output=True, timeout=2)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('UNMOUNT_WITHOUT_OWN_CWD', result.stdout)
                negative = subprocess.run(['/bin/sh'], input='set -eu\n' + functions + block.replace('/newroot', temporary).replace('\ncd /\n', '\n'), text=True, capture_output=True, timeout=2)
                self.assertEqual(negative.returncode, 98)
                self.assertNotIn('UNMOUNT_WITHOUT_OWN_CWD', negative.stdout)


    def test_shadow_lock_accepts_existing_state_but_never_masks_passwd_failure(self):
        source = (ROOT / 'scripts/prepare-installed-qemu-exec-maintenance.sh').read_text()
        block = source.split('# BEGIN EXACT_SHADOW_LOCK\n', 1)[1].split('# END EXACT_SHADOW_LOCK', 1)[0]
        locked = 'veynode:!:::::::\n'
        unlocked = 'veynode:hash:::::::\n'
        cases = [
            ('already_locked', locked, 99, False, True, False),
            ('unlocked_then_locked', unlocked, 0, True, True, True),
            ('passwd_error', unlocked, 13, False, False, True),
            ('passwd_error_even_if_changed', unlocked, 13, True, False, True),
            ('passwd_zero_without_lock', unlocked, 0, False, False, True),
            ('duplicate_locked', locked + locked, 0, True, False, False),
            ('duplicate_mixed', locked + unlocked, 0, True, False, False),
            ('missing_account', 'root:!:::::::\n', 0, True, False, False),
            ('wrong_name', 'veynode-extra:!:::::::\n', 0, True, False, False),
            ('malformed_record', 'veynode:!\n', 0, True, False, False),
        ]
        for name, initial, code, updates, success, called in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / 'etc').mkdir()
                (root / 'etc/shadow').write_text(initial)
                command = 'chroot() { echo LOCK_COMMAND; '
                if updates:
                    command += "printf 'veynode:!:::::::\\n' > '" + str(root / 'etc/shadow') + "'; "
                command += f'return {code}; }}\n'
                result = subprocess.run(['/bin/sh'], input='set -eu\n' + command + block.replace('/newroot', temporary) + '\necho LOCK_VERIFIED\n', text=True, capture_output=True, timeout=2)
                self.assertEqual(result.returncode == 0, success, (name, result.stderr))
                self.assertEqual('LOCK_COMMAND' in result.stdout, called)
                self.assertEqual('LOCK_VERIFIED' in result.stdout, success)


if __name__ == '__main__':
    unittest.main()
