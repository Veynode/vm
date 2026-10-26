#!/usr/bin/env python3
"""Unattended install of a full Linux distribution from its official ISO.

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

The installed systems get user `veynode` (password from --password, default
`veynode`), GDM automatic login, no GNOME first-run wizard, and a small boot
reporter that writes `VEYNODE-BOOTED ...` and `VEYNODE-GL renderer=...` lines to
the serial consoles (ttyAMA0 on QEMU, hvc0 on Apple Virtualization).

Examples (VEYNODEVM_HOME and a running daemon as for any veynodevm command):
  scripts/install-distro.py ubuntu --name ubuntu --iso .local/iso/ubuntu-24.04.5-desktop-arm64.iso \\
      --evidence .local/evidence/ubuntu-installed.png
  scripts/install-distro.py fedora --name fedora \\
      --iso .local/iso/Fedora-Everything-netinst-aarch64-44-1.7.iso \\
      --live-iso .local/iso/Fedora-Workstation-Live-44-1.7.aarch64.iso \\
      --evidence .local/evidence/fedora-installed.png
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


def type_text(qmp, text, delay=0.06):
    for char in text:
        qmp.call('send-key', keys=[{'type': 'qcode', 'data': k} for k in qcodes(char)], **{'hold-time': 40})
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


def wait_stopped(name, timeout, work, label):
    until = time.time() + timeout
    next_shot = 0
    while time.time() < until:
        state = vm_state(name)
        if state in ('stopped', 'crashed'):
            return state
        if time.time() >= next_shot:
            next_shot = time.time() + 120
            try:
                path = screenshot(name, pathlib.Path(work) / f'{label}-{time.strftime("%H%M%S")}.png')
                log(f'{label}: {state}, screen {path}')
            except Exception as e:  # progress screenshots are best effort
                log(f'{label}: {state} (screenshot failed: {e})')
        time.sleep(10)
    raise SystemExit(f'{name} did not finish within {timeout // 60} minutes')


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
    p = sub.add_parser('boot', help='start an installed machine and capture its desktop')
    p.add_argument('--name', required=True)
    p.add_argument('--evidence')
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
