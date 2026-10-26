import base64, importlib.util, json, pathlib, tempfile, unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('install_distro', ROOT / 'scripts/install-distro.py')
install_distro = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install_distro)


class InstallDistroTests(unittest.TestCase):
    def test_grub_command_lines_type_with_us_qcodes(self):
        line = 'linux /images/pxeboot/vmlinuz inst.stage2=hd:LABEL=Fedora-E-dvd-aarch64-44 ro\n'
        codes = [install_distro.qcodes(c) for c in line]
        self.assertEqual(install_distro.qcodes('L'), ['shift', 'l'])
        self.assertEqual(install_distro.qcodes(':'), ['shift', 'semicolon'])
        self.assertEqual(install_distro.qcodes('_'), ['shift', 'minus'])
        self.assertEqual(codes[-1], ['ret'])
        self.assertIn(['spc'], codes)
        with self.assertRaises(ValueError):
            install_distro.qcodes('é')

    def test_autoinstall_seed_is_cloud_config_with_late_script(self):
        with tempfile.TemporaryDirectory() as work:
            fake = mock.patch.object(install_distro, "make_seed", lambda directory, label, files: (label, files))
            self.addCleanup(fake.stop)
            fake.start()
            label, files = install_distro.ubuntu_seed(work, '$6$salt$hash', 'veynode')
            self.assertEqual(label, 'CIDATA')
            self.assertTrue(files['user-data'].startswith('#cloud-config\n'))
            data = json.loads(files['user-data'].split('\n', 1)[1])['autoinstall']
            self.assertEqual(data['identity']['username'], 'veynode')
            self.assertEqual(data['shutdown'], 'poweroff')
            self.assertEqual(data['interactive-sections'], [])
            encoded = data['late-commands'][0].split()[1]
            late = base64.b64decode(encoded).decode()
            self.assertIn('AutomaticLogin=veynode', late)
            self.assertIn('/target/etc/gdm3/custom.conf', late)
            self.assertIn('veynode-boot-report.service', late)
            self.assertIn('console=hvc0', late)
            self.assertIn('instance-id', files['meta-data'])

    def test_kickstart_installs_live_rootfs_only_on_the_primary_disk(self):
        with tempfile.TemporaryDirectory() as work:
            fake = mock.patch.object(install_distro, "make_seed", lambda directory, label, files: (label, files))
            self.addCleanup(fake.stop)
            fake.start()
            label, files = install_distro.fedora_seed(work, '$6$salt$hash', 'veynode', 'Fedora-WS-Live-44')
            ks = files['ks.cfg']
            self.assertEqual(label, 'OEMDRV')
            for line in ('cmdline', 'ignoredisk --only-use=vda', 'poweroff',
                         'liveimg --url=file:///run/veynode-live/LiveOS/squashfs.img',
                         'mount -o ro -t iso9660 /dev/disk/by-label/Fedora-WS-Live-44 /run/veynode-live',
                         'systemctl set-default graphical.target',
                         'cat > /etc/gdm/custom.conf'):
                self.assertIn(line, ks)
            # %post bodies must not contain lines kickstart would read as sections.
            post = ks.split('%post', 1)[1].split('\n%end', 1)[0]
            self.assertFalse([l for l in post.splitlines()[1:] if l.startswith('%')])

    def test_iso_label_reads_primary_volume_descriptor(self):
        with tempfile.TemporaryDirectory() as tmp:
            image = pathlib.Path(tmp) / 'x.iso'
            image.write_bytes(b'\0' * (32768 + 40) + b'MY-LABEL'.ljust(32) + b'\0' * 2048)
            self.assertEqual(install_distro.iso_label(image), 'MY-LABEL')


if __name__ == '__main__':
    unittest.main()
