import base64, importlib.util, json, pathlib, tempfile, unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('install_distro', ROOT / 'scripts/install-distro.py')
install_distro = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install_distro)


class InstallDistroTests(unittest.TestCase):
    def test_windows_answer_file_is_valid_xml_for_arm64(self):
        import xml.etree.ElementTree as ET
        with tempfile.TemporaryDirectory() as work:
            fake = mock.patch.object(install_distro, "make_seed", lambda directory, label, files: (label, files))
            self.addCleanup(fake.stop)
            fake.start()
            label, files = install_distro.windows_answer(work, 'veynode', 'p&ss<w>', 'Windows 11 Pro', 'KEY',
                                                         'ssh-ed25519 AAAAC3Nza+/x= veynodevm-test\n')
        self.assertEqual(label, 'VEYNODEWIN')
        body = files['Autounattend.xml']
        ns = {'u': 'urn:schemas-microsoft-com:unattend'}
        root = ET.fromstring(body)
        self.assertEqual([s.get('pass') for s in root.findall('u:settings', ns)], ['windowsPE', 'offlineServicing', 'specialize', 'oobeSystem'])
        pe = [e.text for e in root.find("u:settings[@pass='windowsPE']", ns).iter('{urn:schemas-microsoft-com:unattend}Path') if 'ARM64' in e.text]
        self.assertIn('D:\\viostor\\w11\\ARM64', pe)
        offline = [e.text for e in root.find("u:settings[@pass='offlineServicing']", ns).iter('{urn:schemas-microsoft-com:unattend}Path')]
        self.assertIn('K:\\vioinput\\w11\\ARM64', offline)
        self.assertFalse(any('viogpudo' in p for p in pe + offline))
        keys = [e.get('{http://schemas.microsoft.com/WMIConfig/2002/State}keyValue') for e in root.iter('{urn:schemas-microsoft-com:unattend}PathAndCredentials')]
        self.assertEqual(len(keys), 3 * 8 + 8 * 8)
        self.assertTrue(all(c.get('processorArchitecture') == 'arm64' for c in root.iter('{urn:schemas-microsoft-com:unattend}component')))
        self.assertEqual(root.find('.//u:InstallFrom/u:MetaData/u:Value', ns).text, 'Windows 11 Pro')
        self.assertEqual(root.find('.//u:LocalAccount/u:Password/u:Value', ns).text, 'p&ss<w>')
        setup = [c.text for c in root.iter('{urn:schemas-microsoft-com:unattend}Path')]
        self.assertTrue(any('BypassTPMCheck' in c for c in setup))
        self.assertTrue(any('BypassSecureBootCheck' in c for c in setup))
        commands = [c.text for c in root.iter('{urn:schemas-microsoft-com:unattend}CommandLine')]
        self.assertTrue(any('NetKVM\\w11\\ARM64\\*.inf' in c for c in commands))
        self.assertIn('powercfg /hibernate off', commands)
        self.assertTrue(commands[-1].startswith('shutdown /s'))
        ssh = commands[-3]
        self.assertIn("OpenSSH.Server*", ssh)
        self.assertLess(len(commands[-2]), 1024)
        self.assertIn("C:\\ProgramData\\ssh\\administrators_authorized_keys 'ssh-ed25519 AAAAC3Nza+/x= veynodevm-test'", ssh)
        self.assertLess(len(ssh), 1024)  # FirstLogonCommands CommandLine limit

    def windows_first_logon(self, **options):
        import xml.etree.ElementTree as ET
        with tempfile.TemporaryDirectory() as work:
            fake = mock.patch.object(install_distro, "make_seed", lambda directory, label, files: (label, files))
            self.addCleanup(fake.stop)
            fake.start()
            _, files = install_distro.windows_answer(work, 'veynode', 'pw', 'Windows 11 Pro', 'KEY', 'ssh-ed25519 K x', **options)
        root = ET.fromstring(files['Autounattend.xml'])
        u = '{urn:schemas-microsoft-com:unattend}'
        logon = root.find(f'.//{u}FirstLogonCommands')
        orders = [int(c.find(f'{u}Order').text) for c in logon]
        self.assertEqual(orders, list(range(1, len(orders) + 1)))
        return [c.find(f'{u}CommandLine').text for c in logon]

    def test_windows_guest_tuning_runs_at_first_logon_unless_opted_out(self):
        commands = self.windows_first_logon()
        tuning = install_distro.GUEST_TUNING
        start = commands.index(tuning[0])
        self.assertEqual(commands[start:start + len(tuning)], tuning)
        # The plan switches first, so the timeouts land on the active plan.
        self.assertLess(start, commands.index('powercfg /change monitor-timeout-ac 0'))
        joined = '\n'.join(tuning)
        for service in ('SysMain', 'WSearch', 'DiagTrack'):
            self.assertIn(f'Set-Service {service} -StartupType Disabled', joined)
            self.assertIn(f'Stop-Service {service}', joined)
        for setting in ('powercfg /setactive SCHEME_MIN',
                        'VisualEffects /v VisualFXSetting /t REG_DWORD /d 2 /f',
                        'Personalize /v EnableTransparency /t REG_DWORD /d 0 /f',
                        'fsutil behavior set DisableDeleteNotify 0',
                        'DeliveryOptimization /v DODownloadMode /t REG_DWORD /d 0 /f'):
            self.assertIn(setting, joined)
        self.assertNotIn('Defender', joined)
        self.assertNotIn('WinDefend', joined)
        self.assertTrue(all(len(c) < 1024 for c in commands))  # CommandLine limit
        self.assertIn('OpenSSH.Server*', commands[-3])
        # The host's key-only administrator, and the logon user left standard.
        self.assertIn('New-LocalUser -Name veynode-admin', commands[-2])
        self.assertIn('Remove-LocalGroupMember -SID S-1-5-32-544', commands[-2])
        self.assertTrue(commands[-1].startswith('shutdown /s'))

        plain = self.windows_first_logon(guest_tuning=False)
        self.assertEqual(plain, [c for c in commands if c not in tuning])
        self.assertIn('powercfg /hibernate off', plain)
        # The balloon service (memory statistics for the host) is integration, not tuning.
        self.assertIn(install_distro.BALLOON_SERVICE, plain)
        self.assertIn(r'Balloon\w11\ARM64\blnsvr.exe', install_distro.BALLOON_SERVICE)
        self.assertTrue(install_distro.BALLOON_SERVICE.endswith(r'blnsvr.exe" -i)'))
        # Native images for Windows PowerShell change no setting: always built.
        self.assertIn(install_distro.NGEN, plain)
        self.assertIn('FrameworkArm64', install_distro.NGEN)

    def test_windows_guest_tuning_is_on_by_default_with_an_opt_out_flag(self):
        argv = ['install-distro.py', 'windows', '--name', 'w', '--iso', 'w.iso', '--drivers-iso', 'v.iso']
        seen = []
        with mock.patch.object(install_distro, 'install_windows', seen.append):
            for extra in ([], ['--no-guest-tuning']):
                with mock.patch('sys.argv', argv + extra):
                    install_distro.main()
        self.assertEqual([a.guest_tuning for a in seen], [True, False])

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
