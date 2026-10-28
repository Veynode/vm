#!/usr/bin/env python3
"""Unattended install of a full Linux distribution or Windows 11 from its official ISO.

Creates a QEMU/HVF UEFI machine (`veynodevm machine create --monitor`), boots
the distribution's own GRUB from the unmodified ISO, types the installer
kernel command line through the private QMP monitor (USB keyboard), lets the
distribution's own installer run unattended, then ejects the media, boots the
installed disk and captures a screenshot of the GNOME desktop.

  ubuntu  Ubuntu Desktop 24.04.x arm64 ISO. Autoinstall (`autoinstall` kernel
          argument) with a NoCloud seed image labelled CIDATA (user-data).
  fedora  Fedora Everything netinstall aarch64 ISO boots Anaconda, which reads
          ks.cfg from a seed image labelled OEMDRV and installs the unmodified
          Fedora Workstation Live ISO's root filesystem (`liveimg`), offline.
  windows Windows 11 ARM64 ISO (the user's own download). Windows Setup reads
          Autounattend.xml from a seed CD-ROM labelled VEYNODEWIN; TPM, Secure
          Boot, RAM and storage checks are bypassed (LabConfig) because the
          machine has no TPM and no Secure Boot keys. The virtio-win ARM64
          drivers (--drivers-iso, required) load in Windows PE, so Setup sees the
          virtio-blk disk, and are injected into the installed system; the
          first logon powers the machine off.

The installed systems get user `veynode` (password from --password, default
`veynode`), GDM automatic login, no GNOME first-run wizard, and a small boot
reporter that writes `VEYNODE-BOOTED ...` and `VEYNODE-GL renderer=...` lines to
the serial consoles (ttyAMA0 on QEMU, hvc0 on Apple Virtualization). Windows
gets local administrator `veynode` with automatic logon; it writes nothing to
the serial console, so its evidence is a desktop screenshot.

Examples (VEYNODEVM_HOME and a running daemon as for any veynodevm command):
  scripts/install-distro.py ubuntu --name ubuntu --iso .local/iso/ubuntu-24.04.5-desktop-arm64.iso \\
      --evidence .local/evidence/ubuntu-installed.png
  scripts/install-distro.py fedora --name fedora \\
      --iso .local/iso/Fedora-Everything-netinst-aarch64-44-1.7.iso \\
      --live-iso .local/iso/Fedora-Workstation-Live-44-1.7.aarch64.iso \\
      --evidence .local/evidence/fedora-installed.png
  scripts/install-distro.py windows --name win11 \\
      --iso ~/Downloads/Windows11_Client_arm64_en-us.iso --drivers-iso virtio-win.iso \\
      --evidence .local/evidence/windows-installed.png
  scripts/install-distro.py screenshot --name ubuntu --output shot.png
"""
import argparse, base64, json, os, pathlib, shutil, socket, subprocess, sys, tempfile, threading, time

ROOT = pathlib.Path(__file__).resolve().parents[1]

# ---------------------------------------------------------------- veynodevm

def veynodevm(*args, check=True):
    exe = os.environ.get('VEYNODEVM') or shutil.which('veynodevm') or str(ROOT / 'target/debug/veynodevm')
    result = subprocess.run([exe, '--json', *args], capture_output=True, text=True)
    out = json.loads(result.stdout) if result.stdout.strip() else {}
    if check and (result.returncode != 0 or (isinstance(out, dict) and 'error' in out)):
        raise SystemExit(f'veynodevm {" ".join(args)} failed: {out.get("error") if isinstance(out, dict) else out} {result.stderr}')
    return out


def inspect(name):
    return veynodevm('machine', 'inspect', name)


def vm_state(name):
    return inspect(name)['vm'].get('state', 'missing')


def runtime(name):
    status = inspect(name)['vm']
    if status.get('state') in (None, 'stopped', 'crashed'):
        raise SystemExit(f'{name} is not running')
    return status['runtime']


def log(message):
    print(time.strftime('%H:%M:%S'), message, flush=True)

# ---------------------------------------------------------------- QMP

class Qmp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(30)
        self.sock.connect(path)
        self.file = self.sock.makefile('rwb')
        self._read()
        self.call('qmp_capabilities')

    def _read(self):
        while True:
            line = self.file.readline()
            if not line:
                raise ConnectionError('QMP monitor closed')
            message = json.loads(line)
            if 'event' not in message:
                return message

    def call(self, command, **arguments):
        self.file.write(json.dumps({'execute': command, 'arguments': arguments}).encode() + b'\n')
        self.file.flush()
        reply = self._read()
        if 'error' in reply:
            raise RuntimeError(f'QMP {command}: {reply["error"]}')
        return reply['return']

    def close(self):
        self.sock.close()


SHIFTED = {'_': 'minus', '+': 'equal', ':': 'semicolon', '"': 'apostrophe', '<': 'comma',
           '>': 'dot', '?': 'slash', '|': 'backslash', '{': 'bracket_left', '}': 'bracket_right',
           '!': '1', '@': '2', '#': '3', '$': '4', '%': '5', '^': '6', '&': '7', '*': '8',
           '(': '9', ')': '0', '~': 'grave_accent'}
PLAIN = {' ': 'spc', '-': 'minus', '=': 'equal', ';': 'semicolon', "'": 'apostrophe', ',': 'comma',
         '.': 'dot', '/': 'slash', '\\': 'backslash', '[': 'bracket_left', ']': 'bracket_right',
         '`': 'grave_accent', '\n': 'ret'}


def qcodes(char):
    """US layout qcodes for one character."""
    if char.isascii() and char.isalpha():
        return ['shift', char.lower()] if char.isupper() else [char]
    if char.isascii() and char.isdigit():
        return [char]
    if char in PLAIN:
        return [PLAIN[char]]
    if char in SHIFTED:
        return ['shift', SHIFTED[char]]
    raise ValueError(f'cannot type {char!r}')


def send_keys(qmp, *combos, delay=0.15):
    for combo in combos:
        qmp.call('send-key', keys=[{'type': 'qcode', 'data': k} for k in combo.split('+')], **{'hold-time': 60})
        time.sleep(delay)


def type_text(qmp, text, delay=0.18):
    # Windows drops keys (and once kept one repeating) at 40 ms holds 60 ms apart.
    for char in text:
        qmp.call('send-key', keys=[{'type': 'qcode', 'data': k} for k in qcodes(char)], **{'hold-time': 80})
        time.sleep(delay)

# ---------------------------------------------------------------- screenshots

def screenshot_helper():
    try:
        helper = json.loads((pathlib.Path(os.environ.get('VEYNODEVM_HOME', pathlib.Path.home() / 'Library/Application Support/VeynodeVM')) / 'runtime.json').read_text())['qemu'].get('screenshot_helper')
    except (OSError, KeyError, ValueError):
        helper = None
    for candidate in (helper, ROOT / '.local/bin/spice-screenshot'):
        if candidate and pathlib.Path(candidate).is_file():
            return str(candidate)
    return None


def spice_capture(state, attempts=6):
    """SPICE GL scanout capture (works for virtio-gpu-gl/virgl, where QMP
    screendump only sees the unused 2D surface). The helper captures on the
    next draw, so nudge the USB tablet while it waits."""
    helper = screenshot_helper()
    if not helper:
        return None
    qmp = Qmp(state['monitor_socket'])
    try:
        for _ in range(attempts):
            proc = subprocess.Popen([helper, state['spice_socket']], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            out = []
            reader = threading.Thread(target=lambda: out.append(proc.stdout.read()))
            reader.start()
            for i in range(48):
                if proc.poll() is not None:
                    break
                time.sleep(0.25)
                qmp.call('input-send-event', events=[
                    {'type': 'abs', 'data': {'axis': 'x', 'value': 32000 + (i % 2) * 64}},
                    {'type': 'abs', 'data': {'axis': 'y', 'value': 31000}}])
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            reader.join()
            try:
                result = json.loads(out[0] or b'{}')
            except ValueError:
                continue
            key = next((k for k in result if 'base64' in k), None)
            if key:
                return base64.b64decode(result[key])
        return None
    finally:
        qmp.close()


def qmp_screendump(state):
    with tempfile.TemporaryDirectory() as tmp:
        ppm = pathlib.Path(tmp) / 'screen.ppm'
        png = pathlib.Path(tmp) / 'screen.png'
        qmp = Qmp(state['monitor_socket'])
        try:
            qmp.call('screendump', filename=str(ppm))
        finally:
            qmp.close()
        subprocess.run(['sips', '-s', 'format', 'png', str(ppm), '--out', str(png)], check=True, capture_output=True)
        return png.read_bytes()


def screenshot(name, output):
    state = runtime(name)
    data = spice_capture(state) or qmp_screendump(state)
    output = pathlib.Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    return output

# ---------------------------------------------------------------- serial log

class Serial:
    def __init__(self, path):
        self.path = pathlib.Path(path)
        self.offset = self.path.stat().st_size if self.path.exists() else 0

    def read(self):
        if not self.path.exists():
            return ''
        with open(self.path, 'rb') as f:
            f.seek(self.offset)
            data = f.read()
        return data.decode('utf-8', 'replace')

    def wait_for(self, needle, timeout, name=None):
        until = time.time() + timeout
        while time.time() < until:
            if needle in self.read():
                return True
            if name and vm_state(name) in ('stopped', 'crashed'):
                raise SystemExit(f'{name} stopped while waiting for {needle!r}')
            time.sleep(1)
        raise SystemExit(f'timed out waiting for {needle!r} on the serial console')

    def advance(self):
        self.offset = self.path.stat().st_size

# ---------------------------------------------------------------- seeds

BOOT_REPORTER = r'''#!/bin/sh
# VeynodeVM boot reporter: proof of boot and of the GNOME session GL renderer
# on the serial consoles (QEMU ttyAMA0, Apple Virtualization hvc0) and kmsg.
say() {
  for dev in /dev/ttyAMA0 /dev/hvc0; do [ -c "$dev" ] && echo "$*" > "$dev" 2>/dev/null; done
  echo "$*" > /dev/kmsg 2>/dev/null
}
. /etc/os-release
addr=$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $2"="$4}' | tr '\n' ' ')
say "VEYNODE-BOOTED os=\"$PRETTY_NAME\" kernel=$(uname -r) addr=$addr"
for i in $(seq 1 180); do
  if [ -s /tmp/veynode-gl-renderer.txt ]; then
    sleep 1
    say "VEYNODE-GL renderer=\"$(head -c 300 /tmp/veynode-gl-renderer.txt | tr '\n' ' ')\" session=\"$(loginctl list-sessions --no-legend 2>/dev/null | tr -s ' ' | tr '\n' ';')\""
    exit 0
  fi
  sleep 2
done
say "VEYNODE-GL renderer=unavailable session=\"$(loginctl list-sessions --no-legend 2>/dev/null | tr -s ' ' | tr '\n' ';')\""
'''

BOOT_REPORTER_UNIT = '''[Unit]
Description=VeynodeVM boot reporter (serial console markers)
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/sbin/veynode-boot-report

[Install]
WantedBy=multi-user.target
'''

# Runs inside the GNOME session: the GL probe gnome-session (<= 49) uses to
# decide whether the session is accelerated, else glxinfo when installed.
GL_PROBE = r'''#!/bin/sh
sleep 5
out=/tmp/veynode-gl-renderer.tmp
: > "$out"
for h in /usr/libexec/gnome-session-check-accelerated-gl-helper /usr/lib/gnome-session/gnome-session-check-accelerated-gl-helper; do
  [ -x "$h" ] && "$h" --print-renderer > "$out" 2>/dev/null && break
done
if [ ! -s "$out" ] && command -v glxinfo >/dev/null; then
  glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' > "$out"
fi
[ -s "$out" ] || echo unknown > "$out"
mv "$out" /tmp/veynode-gl-renderer.txt
'''

GL_AUTOSTART = '''[Desktop Entry]
Type=Application
Name=VeynodeVM GL renderer report
Exec=/usr/local/bin/veynode-gl-probe
NoDisplay=true
X-GNOME-Autostart-Phase=Application
'''

# Fedora shows the GNOME Tour on first login; mark it as already shown.
FEDORA_DCONF = '''[org/gnome/shell]
welcome-dialog-last-shown-version='999999'
'''


def crypt_password(password):
    out = subprocess.run(['openssl', 'passwd', '-6', '-stdin'], input=password, capture_output=True, text=True, check=True)
    return out.stdout.strip()


def make_seed(directory, label, files):
    """ISO 9660 + Joliet seed image with the given volume label."""
    directory = pathlib.Path(directory)
    source = directory / f'{label.lower()}-files'
    if source.exists():
        shutil.rmtree(source)
    source.mkdir(parents=True)
    for name, body in files.items():
        (source / name).write_text(body)
    image = directory / f'{label.lower()}.iso'
    if image.exists():
        image.unlink()
    subprocess.run(['hdiutil', 'makehybrid', '-quiet', '-iso', '-joliet', '-default-volume-name', label,
                    '-o', str(image), str(source)], check=True)
    os.chmod(image, 0o600)
    return image


def install_files_script(prefix, gdm_conf, user):
    """Shell commands (run with the target root at `prefix`) that add autologin,
    skip the first-run wizard and install the boot reporter."""
    def heredoc(path, body, mode='644'):
        return f"mkdir -p \"$(dirname {prefix}{path})\"\ncat > {prefix}{path} <<'VEYNODE_EOF'\n{body}VEYNODE_EOF\nchmod {mode} {prefix}{path}\n"
    return ''.join([
        heredoc(gdm_conf, f'[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin={user}\n'),
        heredoc('/etc/skel/.config/gnome-initial-setup-done', 'yes\n'),
        f'if [ -d {prefix}/home/{user} ]; then mkdir -p {prefix}/home/{user}/.config && echo yes > {prefix}/home/{user}/.config/gnome-initial-setup-done && chown -R --reference={prefix}/home/{user} {prefix}/home/{user}/.config; fi\n',
        heredoc('/usr/local/sbin/veynode-boot-report', BOOT_REPORTER, '755'),
        heredoc('/etc/systemd/system/veynode-boot-report.service', BOOT_REPORTER_UNIT),
        f'mkdir -p {prefix}/etc/systemd/system/multi-user.target.wants\n',
        f'ln -sf /etc/systemd/system/veynode-boot-report.service {prefix}/etc/systemd/system/multi-user.target.wants/veynode-boot-report.service\n',
        heredoc('/usr/local/bin/veynode-gl-probe', GL_PROBE, '755'),
        heredoc('/etc/xdg/autostart/veynode-gl-report.desktop', GL_AUTOSTART),
    ])


def ubuntu_seed(work, password_hash, user):
    script = install_files_script('/target', '/etc/gdm3/custom.conf', user)
    # Serial consoles for both backends; tty0 last keeps the graphical boot.
    script += ("sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT=.*/GRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash console=ttyAMA0 console=hvc0 console=tty0\"/' /target/etc/default/grub\n"
               "curtin in-target --target=/target -- update-grub\n")
    (pathlib.Path(work) / 'late.sh').write_text(script)
    user_data = {
        'autoinstall': {
            'version': 1,
            'interactive-sections': [],
            'source': {'id': 'ubuntu-desktop-minimal', 'search_drivers': False},
            'locale': 'en_US.UTF-8',
            'keyboard': {'layout': 'us'},
            'timezone': 'Etc/UTC',
            'identity': {'hostname': 'ubuntu-veynode', 'realname': 'VeynodeVM', 'username': user, 'password': password_hash},
            'storage': {'layout': {'name': 'direct'}},
            'codecs': {'install': False},
            'drivers': {'install': False},
            'oem': {'install': False},
            'apt': {'geoip': False, 'fallback': 'offline-install'},
            'updates': 'security',
            # The late script travels inside user-data (no second mount needed).
            'late-commands': [
                f"echo {base64.b64encode(script.encode()).decode()} | base64 -d > /tmp/veynode-late.sh",
                'sh -x /tmp/veynode-late.sh > /var/log/veynode-late.log 2>&1',
            ],
            'shutdown': 'poweroff',
        }
    }
    body = '#cloud-config\n' + json.dumps(user_data, indent=1) + '\n'
    return make_seed(work, 'CIDATA', {'user-data': body, 'meta-data': 'instance-id: veynode-ubuntu-install\n'})


def fedora_seed(work, password_hash, user, live_label):
    post = install_files_script('', '/etc/gdm/custom.conf', user)
    post += ("mkdir -p /etc/dconf/db/local.d\n"
             f"cat > /etc/dconf/db/local.d/00-veynode <<'VEYNODE_EOF'\n{FEDORA_DCONF}VEYNODE_EOF\n"
             "dconf update\n")
    kickstart = f'''# VeynodeVM unattended Fedora Workstation install (offline, from the Live ISO)
cmdline
lang en_US.UTF-8
keyboard us
timezone Etc/UTC --utc
network --bootproto=dhcp --device=link --activate --onboot=yes --hostname=fedora-veynode
rootpw --lock
user --name={user} --groups=wheel --gecos="VeynodeVM" --iscrypted --password={password_hash}
ignoredisk --only-use=vda
zerombr
clearpart --all --initlabel --drives=vda --disklabel=gpt
autopart --type=btrfs
bootloader --append="console=ttyAMA0 console=hvc0 console=tty0"
liveimg --url=file:///run/veynode-live/LiveOS/squashfs.img
firstboot --disable
poweroff

%pre --log=/tmp/veynode-pre.log
mkdir -p /run/veynode-live
mount -o ro -t iso9660 /dev/disk/by-label/{live_label} /run/veynode-live
%end

%post --log=/root/veynode-post.log
# Text-mode (cmdline) installs default to multi-user.target; boot to GDM.
systemctl set-default graphical.target
{post}%end
'''
    return make_seed(work, 'OEMDRV', {'ks.cfg': kickstart})


# Generic (non-activating) installation keys that select an edition.
WINDOWS_KEYS = {'Windows 11 Pro': 'VK7JG-NPHTM-C97JM-9MPGT-3V66T', 'Windows 11 Home': 'YTMG3-N6DKC-DKB77-7M9GH-8HVX7'}


def xml_text(value):
    return value.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;').replace('"', '&quot;')


# virtio-win ARM64 driver directories (virtio-win 0.1.302 layout). viogpudo is
# left out (virtio-ramfb stays on Basic Display), as are viosock/viofs/viomem
# (no vhost-vsock, virtiofsd or virtio-mem on this host).
VIRTIO_DRIVERS = ['viostor', 'vioscsi', 'NetKVM', 'vioserial', 'Balloon', 'viorng', 'vioinput', 'pvpanic']
# Windows PE and Setup letter CD-ROMs from D:, in an order we don't control.
DRIVE_LETTERS = 'DEFGHIJK'


def driver_paths(drivers):
    return [f'{letter}:\\{driver}\\w11\\ARM64' for driver in drivers for letter in DRIVE_LETTERS]


def powershell(script):
    return f'powershell -NoProfile -ExecutionPolicy Bypass -Command "{script}"'


def openssh_server(authorized_key):
    """PowerShell for FirstLogonCommands: Windows' OpenSSH Server (a Windows
    Update download), the key for administrators, PowerShell as the shell."""
    keys = 'C:\\ProgramData\\ssh\\administrators_authorized_keys'
    script = '; '.join([
        "Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Add-WindowsCapability -Online",
        'Set-Service sshd -StartupType Automatic', 'Start-Service sshd',
        "New-NetFirewallRule -Name veynode-sshd -DisplayName 'OpenSSH (VeynodeVM)' -Protocol TCP -LocalPort 22 -Action Allow",
        f"Set-Content -Encoding ascii {keys} '{authorized_key.strip()}'",
        f'icacls {keys} /inheritance:r /grant Administrators:F /grant SYSTEM:F',
        "New-ItemProperty -Path HKLM:\\SOFTWARE\\OpenSSH -Name DefaultShell -PropertyType String -Force "
        "-Value C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
    ])
    return powershell(script)


def disable_service(name):
    return powershell(f'Set-Service {name} -StartupType Disabled -ErrorAction SilentlyContinue; '
                      f'Stop-Service {name} -Force -ErrorAction SilentlyContinue')


# Guest tuning usual for development and test VMs (--no-guest-tuning leaves
# Windows' defaults). Each command is idempotent and runs at first logon as
# the automatic-logon user, so HKCU is that user's. Windows Defender stays on.
GUEST_TUNING = [
    # High performance plan (powercfg's SCHEME_MIN alias). First, so the
    # display and sleep timeouts below are set on the plan that is active.
    'powercfg /setactive SCHEME_MIN',
    # Prefetching (SysMain) and the search indexer (WSearch) spend the
    # virtual disk and CPU on work a short-lived VM does not benefit from.
    disable_service('SysMain'),
    disable_service('WSearch'),
    # Telemetry: the Connected User Experiences and Telemetry service.
    disable_service('DiagTrack'),
    # Delivery Optimization: HTTP downloads only, no peer-to-peer uploads.
    'reg add HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows\\DeliveryOptimization /v DODownloadMode /t REG_DWORD /d 0 /f',
    # Visual effects "Adjust for best performance": the setting, the effects
    # mask it stands for and no minimize animation (they apply from the next
    # logon). Font smoothing stays on so screenshots stay legible.
    'reg add HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\VisualEffects /v VisualFXSetting /t REG_DWORD /d 2 /f',
    'reg add "HKCU\\Control Panel\\Desktop" /v UserPreferencesMask /t REG_BINARY /d 9012038010000000 /f',
    'reg add "HKCU\\Control Panel\\Desktop\\WindowMetrics" /v MinAnimate /t REG_SZ /d 0 /f',
    'reg add HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize /v EnableTransparency /t REG_DWORD /d 0 /f',
    # TRIM (the default, made sure of): the host passes discard through
    # virtio-blk, so freed blocks shrink the disk image.
    'fsutil behavior set DisableDeleteNotify 0',
]


NGEN = r'C:\Windows\Microsoft.NET\FrameworkArm64\v4.0.30319\ngen.exe executeQueuedItems /nologo'


def windows_answer(work, user, password, edition, product_key, authorized_key=None, guest_tuning=True):
    component = lambda name: (f'<component name="{name}" processorArchitecture="arm64" publicKeyToken="31bf3856ad364e35" '
                              'language="neutral" versionScope="nonSxS">')
    def commands(tag, element, lines):
        body = ''.join(f'<{element} wcm:action="add"><Order>{i}</Order>'
                       f'<{"CommandLine" if element == "SynchronousCommand" else "Path"}>{xml_text(line)}'
                       f'</{"CommandLine" if element == "SynchronousCommand" else "Path"}></{element}>'
                       for i, line in enumerate(lines, 1))
        return f'<{tag}>{body}</{tag}>'
    def paths(drivers):
        return '<DriverPaths>' + ''.join(
            f'<PathAndCredentials wcm:action="add" wcm:keyValue="{i}"><Path>{xml_text(path)}</Path></PathAndCredentials>'
            for i, path in enumerate(driver_paths(drivers), 1)) + '</DriverPaths>'
    # No TPM, no Secure Boot keys: tell Setup to skip the Windows 11 checks.
    labconfig = [f'reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {value} /t REG_DWORD /d 1 /f'
                 for value in ('BypassTPMCheck', 'BypassSecureBootCheck', 'BypassRAMCheck', 'BypassStorageCheck', 'BypassCPUCheck')]
    first_logon = [
        # Fallback for any virtio driver offline servicing did not install.
        *[f'cmd /c for %d in ({" ".join(DRIVE_LETTERS)}) do @if exist %d:\\{d}\\w11\\ARM64 pnputil /add-driver %d:\\{d}\\w11\\ARM64\\*.inf /install'
          for d in VIRTIO_DRIVERS],
        *(GUEST_TUNING if guest_tuning else []),
        # Compile the queued .NET Framework assemblies for ARM64 now (~30 s),
        # not whenever Windows next idles: until then every Windows PowerShell
        # start, and so every exec, desktop or file call, JIT-compiles for
        # ~1 s (with native images ~0.2 s). It changes no setting.
        NGEN,
        'powercfg /change monitor-timeout-ac 0',
        'powercfg /change standby-timeout-ac 0',
        # No Fast Startup: QEMU/HVF aborts (assert isv) resuming its hybrid
        # shutdown image once the Neptune GPU driver is installed.
        'powercfg /hibernate off',
        *([openssh_server(authorized_key)] if authorized_key else []),
        # Powering off tells the host the install is complete.
        'shutdown /s /t 20 /c "VeynodeVM install complete"',
    ]
    # Local account without a network: no "connect to the internet" page.
    specialize = ['reg add HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\OOBE /v BypassNRO /t REG_DWORD /d 1 /f']
    user, password = xml_text(user), xml_text(password)
    xml = f'''<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
<settings pass="windowsPE">
{component('Microsoft-Windows-International-Core-WinPE')}
<SetupUILanguage><UILanguage>en-US</UILanguage></SetupUILanguage>
<InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale>
</component>
{component('Microsoft-Windows-PnpCustomizationsWinPE')}{paths(['viostor', 'vioscsi', 'NetKVM'])}</component>
{component('Microsoft-Windows-Setup')}
{commands('RunSynchronous', 'RunSynchronousCommand', labconfig)}
<DiskConfiguration><Disk wcm:action="add"><DiskID>0</DiskID><WillWipeDisk>true</WillWipeDisk>
<CreatePartitions>
<CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>260</Size></CreatePartition>
<CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
<CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
</CreatePartitions>
<ModifyPartitions>
<ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Format>FAT32</Format><Label>System</Label></ModifyPartition>
<ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>3</PartitionID><Format>NTFS</Format><Label>Windows</Label><Letter>C</Letter></ModifyPartition>
</ModifyPartitions>
</Disk></DiskConfiguration>
<ImageInstall><OSImage>
<InstallFrom><MetaData wcm:action="add"><Key>/IMAGE/NAME</Key><Value>{xml_text(edition)}</Value></MetaData></InstallFrom>
<InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
</OSImage></ImageInstall>
<UserData><ProductKey><Key>{xml_text(product_key)}</Key><WillShowUI>Never</WillShowUI></ProductKey><AcceptEula>true</AcceptEula></UserData>
</component>
</settings>
<settings pass="offlineServicing">
{component('Microsoft-Windows-PnpCustomizationsNonWinPE')}{paths(VIRTIO_DRIVERS)}</component>
</settings>
<settings pass="specialize">
{component('Microsoft-Windows-Shell-Setup')}<ComputerName>WIN-VEYNODE</ComputerName><TimeZone>UTC</TimeZone></component>
{component('Microsoft-Windows-Deployment')}
{commands('RunSynchronous', 'RunSynchronousCommand', specialize)}
</component>
</settings>
<settings pass="oobeSystem">
{component('Microsoft-Windows-International-Core')}<InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale></component>
{component('Microsoft-Windows-Shell-Setup')}
<OOBE><HideEULAPage>true</HideEULAPage><HideOEMRegistrationScreen>true</HideOEMRegistrationScreen><HideOnlineAccountScreens>true</HideOnlineAccountScreens><HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE><ProtectYourPC>3</ProtectYourPC></OOBE>
<UserAccounts><LocalAccounts><LocalAccount wcm:action="add"><Name>{user}</Name><DisplayName>VeynodeVM</DisplayName><Group>Administrators</Group>
<Password><Value>{password}</Value><PlainText>true</PlainText></Password></LocalAccount></LocalAccounts></UserAccounts>
<AutoLogon><Enabled>true</Enabled><Username>{user}</Username><LogonCount>9999999</LogonCount><Password><Value>{password}</Value><PlainText>true</PlainText></Password></AutoLogon>
{commands('FirstLogonCommands', 'SynchronousCommand', first_logon)}
</component>
</settings>
</unattend>
'''
    return make_seed(work, 'VEYNODEWIN', {'Autounattend.xml': xml})


def iso_label(path):
    with open(path, 'rb') as f:
        f.seek(32768 + 40)
        return f.read(32).decode('ascii', 'replace').strip()

# ---------------------------------------------------------------- flow

def grub_command_line(name, serial, commands, menu_marker):
    log('waiting for the ISO\'s GRUB menu (serial console)')
    serial.wait_for(menu_marker, 300, name)
    qmp = Qmp(runtime(name)['monitor_socket'])
    try:
        send_keys(qmp, 'c')
        time.sleep(1.5)
        for line in commands:
            log(f'grub> {line}')
            type_text(qmp, line + '\n')
            time.sleep(1.5)
    finally:
        qmp.close()


def vcpu_pcs(name):
    """Program counters of every vCPU (HMP `info registers -a`)."""
    qmp = Qmp(runtime(name)['monitor_socket'])
    try:
        out = qmp.call('human-monitor-command', **{'command-line': 'info registers -a'})
    finally:
        qmp.close()
    return tuple(line.split('PC=')[1].split()[0] for line in out.splitlines() if 'PC=' in line)


def wait_stopped(name, timeout, work, label, hang_checks=0):
    """Wait for the guest to power off. With hang_checks, stop early when no
    vCPU program counter moves across that many samples two minutes apart
    (a firmware or kernel spin, as seen with a corrupt UEFI variable store)."""
    until = time.time() + timeout
    next_shot = 0
    pcs, still = None, 0
    while time.time() < until:
        state = vm_state(name)
        if state in ('stopped', 'crashed'):
            return state
        if time.time() >= next_shot:
            next_shot = time.time() + 120
            if hang_checks:
                try:
                    now = vcpu_pcs(name)
                except Exception:  # monitor briefly unavailable across a reset
                    now = None
                still = still + 1 if now and now == pcs else 0
                pcs = now
                if still >= hang_checks:
                    raise SystemExit(f'{name} looks hung: vCPU PCs {pcs} unchanged for {still * 2} minutes')
            try:
                path = screenshot(name, pathlib.Path(work) / f'{label}-{time.strftime("%H%M%S")}.png')
                log(f'{label}: {state}, screen {path}')
            except Exception as e:  # progress screenshots are best effort
                log(f'{label}: {state} (screenshot failed: {e})')
        time.sleep(10)
    raise SystemExit(f'{name} did not finish within {timeout // 60} minutes')


def press_any_key(name, serial, seconds=8):
    """Windows' CD boot loader waits a few seconds for a key, else falls
    through to the next boot entry. Its prompt is drawn only on the screen,
    so tap Enter from the moment edk2 starts the first boot entry (the CD).
    Stop soon: a late Enter lands on Setup's focused Cancel button."""
    log('waiting for edk2 to start the installer CD (serial console)')
    serial.wait_for('BdsDxe: starting Boot', 300, name)
    log('pressing Enter for "Press any key to boot from CD or DVD"')
    qmp = Qmp(runtime(name)['monitor_socket'])
    try:
        until = time.time() + seconds
        while time.time() < until:
            send_keys(qmp, 'ret', delay=0.5)
    finally:
        qmp.close()


def install_windows(args):
    work = pathlib.Path(args.work_dir or ROOT / '.local/install' / args.name).resolve()
    work.mkdir(parents=True, exist_ok=True)
    os.chmod(work, 0o700)
    key = args.product_key or WINDOWS_KEYS.get(args.edition)
    if not key:
        raise SystemExit(f'--product-key required for edition {args.edition!r}')
    ssh_key = None
    if args.ssh_port:
        ssh_key = pathlib.Path(args.ssh_key or work / 'id_ed25519')
        if not ssh_key.exists():
            subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-C', f'veynodevm-{args.name}', '-f', str(ssh_key)], check=True)
    authorized_key = ssh_key and pathlib.Path(f'{ssh_key}.pub').read_text()
    seed = windows_answer(work, args.user, args.password, args.edition, key, authorized_key, args.guest_tuning)
    create = ['machine', 'create', args.name, '--os', 'windows', '--iso', str(pathlib.Path(args.iso).expanduser().resolve()),
              '--disk-size', args.disk_size or '64G', '--memory', args.memory, '--cpus', str(args.cpus), '--monitor',
              '--attach', str(seed)]
    if args.ssh_port:
        create += ['--publish', f'{args.ssh_port}:22']
    create += ['--attach', str(pathlib.Path(args.drivers_iso).expanduser().resolve())]
    log(f'creating machine {args.name}')
    veynodevm(*create)
    serial = Serial(inspect(args.name)['vm']['config']['serial_log'])
    started = time.time()
    veynodevm('machine', 'start', args.name)
    press_any_key(args.name, serial)
    log('Windows Setup running unattended; waiting for the first logon to power the machine off')
    state = wait_stopped(args.name, args.timeout_minutes * 60, work, 'install', hang_checks=2)
    if state != 'stopped':
        raise SystemExit(f'installer VM ended in state {state}')
    log(f'install finished in {(time.time() - started) / 60:.1f} minutes')
    veynodevm('machine', 'eject', args.name)
    boot_windows(args.name, args.evidence, work)
    if ssh_key:
        ssh = ['ssh', '-i', str(ssh_key), '-p', str(args.ssh_port), '-o', f'UserKnownHostsFile={work / "known_hosts"}',
               '-o', 'StrictHostKeyChecking=accept-new', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', f'{args.user}@127.0.0.1']
        result = subprocess.run([*ssh, '(Get-CimInstance Win32_OperatingSystem).Caption'], capture_output=True, text=True)
        if result.returncode:
            raise SystemExit(f'ssh into {args.name} failed: {result.stderr.strip()}')
        log(f'ssh: {result.stdout.strip()}; connect with: {" ".join(ssh)}')
        # The daemon's vm_exec, file and desktop tools log in with this key.
        veynodevm('machine', 'ssh-key', args.name, '--user', args.user, '--key', str(ssh_key))


def boot_windows(name, evidence, work, settle=240):
    log('booting installed Windows from its disk')
    veynodevm('machine', 'start', name)
    # No serial marker: give boot and automatic logon time, then capture.
    until = time.time() + settle
    while time.time() < until:
        if vm_state(name) in ('stopped', 'crashed'):
            raise SystemExit(f'{name} stopped while booting')
        time.sleep(10)
    output = pathlib.Path(evidence or pathlib.Path(work) / f'{name}-installed.png')
    screenshot(name, output)
    log(f'desktop screenshot: {output}')


def install(args):
    work = pathlib.Path(args.work_dir or ROOT / '.local/install' / args.name).resolve()
    work.mkdir(parents=True, exist_ok=True)
    os.chmod(work, 0o700)
    password_hash = crypt_password(args.password)
    attach = []
    if args.distro == 'ubuntu':
        seed = ubuntu_seed(work, password_hash, args.user)
        grub = ['linux /casper/vmlinuz autoinstall --- console=tty0', 'initrd /casper/initrd', 'boot']
        marker = 'Try or Install Ubuntu'
    else:
        if not args.live_iso:
            raise SystemExit('fedora needs --live-iso (Fedora Workstation Live aarch64 ISO)')
        live = pathlib.Path(args.live_iso).resolve()
        seed = fedora_seed(work, password_hash, args.user, iso_label(live))
        attach.append(live)
        label = iso_label(args.iso)
        grub = [f'linux /images/pxeboot/vmlinuz inst.stage2=hd:LABEL={label} inst.ks=hd:LABEL=OEMDRV:/ks.cfg console=tty0 console=ttyAMA0 ro',
                'initrd /images/pxeboot/initrd.img', 'boot']
        marker = 'Install Fedora'
    attach.insert(0, seed)
    create = ['machine', 'create', args.name, '--iso', str(pathlib.Path(args.iso).resolve()), '--disk-size', args.disk_size,
              '--memory', args.memory, '--cpus', str(args.cpus), '--gpus', args.gpus, '--monitor']
    for image in attach:
        create += ['--attach', str(image)]
    log(f'creating machine {args.name}')
    veynodevm(*create)
    config = inspect(args.name)['vm']['config']
    serial = Serial(config['serial_log'])
    started = time.time()
    veynodevm('machine', 'start', args.name)
    grub_command_line(args.name, serial, grub, marker)
    log('installer running unattended; waiting for it to power the machine off')
    state = wait_stopped(args.name, args.timeout_minutes * 60, work, 'install')
    if state != 'stopped':
        raise SystemExit(f'installer VM ended in state {state}; see {config["serial_log"]}')
    log(f'install finished in {(time.time() - started) / 60:.1f} minutes')
    veynodevm('machine', 'eject', args.name)
    boot_installed(args.name, args.evidence, work)


def boot_installed(name, evidence, work):
    config = inspect(name)['vm']['config']
    serial = Serial(config['serial_log'])
    log('booting the installed system from its disk')
    veynodevm('machine', 'start', name)
    serial.wait_for('VEYNODE-BOOTED', 600, name)
    booted = next(l for l in serial.read().splitlines() if 'VEYNODE-BOOTED' in l)
    log(booted.strip())
    serial.wait_for('VEYNODE-GL', 600, name)
    gl = next(l for l in serial.read().splitlines() if 'VEYNODE-GL' in l)
    log(gl.strip())
    time.sleep(15)
    output = pathlib.Path(evidence or pathlib.Path(work) / f'{name}-installed.png')
    screenshot(name, output)
    (output.with_suffix('.txt')).write_text(booted.strip() + '\n' + gl.strip() + '\n')
    log(f'desktop screenshot: {output}')


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    sub = parser.add_subparsers(dest='distro', required=True)
    for distro in ('ubuntu', 'fedora'):
        p = sub.add_parser(distro)
        p.add_argument('--name', required=True)
        p.add_argument('--iso', required=True)
        p.add_argument('--live-iso', help='fedora: Workstation Live ISO whose root filesystem is installed')
        p.add_argument('--disk-size', default='32G')
        p.add_argument('--memory', default='6g')
        p.add_argument('--cpus', type=int, default=4)
        p.add_argument('--gpus', default='virgl')
        p.add_argument('--user', default='veynode')
        p.add_argument('--password', default='veynode')
        p.add_argument('--timeout-minutes', type=int, default=120)
        p.add_argument('--work-dir')
        p.add_argument('--evidence', help='PNG path for the installed desktop screenshot')
    p = sub.add_parser('windows')
    p.add_argument('--name', required=True)
    p.add_argument('--iso', required=True, help='Windows 11 ARM64 ISO')
    p.add_argument('--drivers-iso', required=True, help='virtio-win ISO (ARM64 virtio drivers; 0.1.302 verified)')
    p.add_argument('--edition', default='Windows 11 Pro', help='install.wim image name')
    p.add_argument('--product-key', help='defaults to the generic key for --edition')
    p.add_argument('--disk-size', default='64G')
    p.add_argument('--memory', default='8g')
    p.add_argument('--cpus', type=int, default=4)
    p.add_argument('--user', default='veynode')
    p.add_argument('--password', default='veynode')
    p.add_argument('--timeout-minutes', type=int, default=180)
    p.add_argument('--ssh-port', type=int, default=2222, help='host port forwarded to the guest OpenSSH Server (0: none)')
    p.add_argument('--ssh-key', help='private key whose .pub is authorized (default: generated in the work dir)')
    p.add_argument('--no-guest-tuning', dest='guest_tuning', action='store_false',
                   help="keep Windows' defaults: no High performance plan, SysMain/WSearch/DiagTrack stay on, "
                        'visual effects, transparency and Delivery Optimization unchanged')
    p.add_argument('--work-dir')
    p.add_argument('--evidence', help='PNG path for the installed desktop screenshot')
    p = sub.add_parser('boot', help='start an installed machine and capture its desktop')
    p.add_argument('--name', required=True)
    p.add_argument('--evidence')
    p.add_argument('--windows', action='store_true', help='no serial markers: wait, then capture')
    p = sub.add_parser('screenshot')
    p.add_argument('--name', required=True)
    p.add_argument('--output', required=True)
    p = sub.add_parser('type', help='type text (\\n = Enter) into a running machine')
    p.add_argument('--name', required=True)
    p.add_argument('text')
    p = sub.add_parser('keys', help='send key combos, e.g. ctrl+alt+f3 ret')
    p.add_argument('--name', required=True)
    p.add_argument('combos', nargs='+')
    args = parser.parse_args()
    if args.distro in ('ubuntu', 'fedora'):
        install(args)
    elif args.distro == 'windows':
        install_windows(args)
    elif args.distro == 'boot' and args.windows:
        boot_windows(args.name, args.evidence, ROOT / '.local/install' / args.name)
    elif args.distro == 'boot':
        boot_installed(args.name, args.evidence, ROOT / '.local/install' / args.name)
    elif args.distro == 'screenshot':
        print(screenshot(args.name, args.output))
    else:
        qmp = Qmp(runtime(args.name)['monitor_socket'])
        try:
            if args.distro == 'type':
                type_text(qmp, args.text.encode().decode('unicode_escape'))
            else:
                send_keys(qmp, *args.combos)
        finally:
            qmp.close()


if __name__ == '__main__':
    main()
