//! The unattended Windows 11 install behind `machine.create` with
//! `unattended`, in the daemon (it was `scripts/install-distro.py windows`).
//!
//! Windows Setup reads Autounattend.xml from a seed CD-ROM labelled
//! VEYNODEWIN. TPM, Secure Boot, RAM and storage checks are bypassed
//! (LabConfig) because the machine has no TPM and no Secure Boot keys. The
//! virtio-win ARM64 drivers load in Windows PE, so Setup sees the virtio-blk
//! disk, and are injected into the installed system, with viogpudo for the
//! display unless the machine uses Neptune. The first logon turns
//! Fast Startup off, installs OpenSSH Server with this machine's key for the
//! administrator, and powers the machine off: that ends the install, and
//! `machine.create` ends with the machine stopped.
//!
//! OpenSSH comes from the seed ([`OPENSSH`], which the daemon downloads once):
//! Windows' own optional capability is a Windows Update download that takes
//! ten of an install's twenty-five minutes, and stays the fallback.
//!
//! With `gpu` `neptune`, the first logon also installs viogpu3d, the Neptune
//! guest driver, from UTM's guest tools ([`GUEST_TOOLS`]), which the daemon
//! downloads and attaches as another CD-ROM. Its catalog is signed through
//! Microsoft's hardware program, so `pnputil` installs it without a prompt.
use super::progress;
use crate::home;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// The desktop user the answer file creates and Windows signs in by itself:
/// a standard user once the install ends, so what runs on its desktop
/// cannot read what only administrators can.
pub const USER: &str = "veynode";
const PASSWORD: &str = "veynode";
/// The administrator the host reaches the machine as over SSH, by key only:
/// its password is random, made in the guest and never kept, and it is
/// hidden from the sign-in screen.
pub const ADMIN: &str = "veynode-admin";
const EDITION: &str = "Windows 11 Pro";
/// Microsoft's generic key for the edition: it installs, it does not activate.
const PRODUCT_KEY: &str = "VK7JG-NPHTM-C97JM-9MPGT-3V66T";

/// virtio-win ARM64 driver directories (virtio-win 0.1.302 layout).
/// viosock/viofs/viomem are left out (no vhost-vsock, virtiofsd or
/// virtio-mem on this host); the display driver is [`DISPLAY_DRIVER`].
const VIRTIO_DRIVERS: [&str; 8] = [
    "viostor",
    "vioscsi",
    "NetKVM",
    "vioserial",
    "Balloon",
    "viorng",
    "vioinput",
    "pvpanic",
];
/// virtio-win's display-only driver for `virtio-ramfb` without Neptune:
/// 1280x800 and a hardware pointer the viewer draws itself, where Basic
/// Display gives 800x600 with the pointer painted into the picture. Neptune
/// machines get viogpu3d instead.
const DISPLAY_DRIVER: &str = "viogpudo";
/// UTM's guest tools 0.1.274, the release whose viogpu3d speaks Neptune wire
/// format 2, as UTM 5.0.8's runtime does (0.1.273 speaks 1).
/// Downloaded from UTM's QEMU release, not shipped.
pub const GUEST_TOOLS: veynodevm_oci::download::Pinned = veynodevm_oci::download::Pinned {
    url:
        "https://github.com/utmapp/qemu/releases/download/v10.0.12-utm/utm-guest-tools-0.1.274.iso",
    size: 143_898_624,
    sha256: "ab6add47a4ed992f5920f14e96f68d60566e6f731e2cf816c3d38ab2b78f407a",
};
/// The Neptune driver's directory on the guest tools CD-ROM.
const NEPTUNE_DRIVER: &str = "Drivers\\viogpu3d\\w11\\ARM64";

/// [`GUEST_TOOLS`] in the home's download cache, fetched the first time.
pub fn guest_tools() -> Result<PathBuf> {
    let path =
        home::private_dir(&home::root()?.join("downloads"))?.join("utm-guest-tools-0.1.274.iso");
    veynodevm_oci::download::fetch(&GUEST_TOOLS, &path, &mut |m| progress(m))?;
    Ok(path)
}

/// Microsoft's Win32-OpenSSH installer for ARM64, the source of Windows' own
/// capability. Downloaded from its release, not shipped.
pub const OPENSSH: veynodevm_oci::download::Pinned = veynodevm_oci::download::Pinned {
    url: "https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.0.0.0p2-Preview/OpenSSH-ARM64-v10.0.0.0.msi",
    size: 6_295_552,
    sha256: "7a17d0e22d004fb47ca4bfd8fef926fa305de4ebf70a6f3c7a29c39aabef0023",
};
/// [`OPENSSH`]'s name on the seed.
const OPENSSH_MSI: &str = "OpenSSH.msi";

/// [`OPENSSH`] in the home's download cache, fetched the first time.
fn openssh() -> Result<PathBuf> {
    let path =
        home::private_dir(&home::root()?.join("downloads"))?.join("OpenSSH-ARM64-v10.0.0.0.msi");
    veynodevm_oci::download::fetch(&OPENSSH, &path, &mut |m| progress(m))?;
    Ok(path)
}

/// Windows PE and Setup letter CD-ROMs from D:, in an order we don't control.
const DRIVE_LETTERS: &str = "DEFGHIJK";

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn driver_paths(drivers: &[&str]) -> Vec<String> {
    drivers
        .iter()
        .flat_map(|driver| {
            DRIVE_LETTERS
                .chars()
                .map(move |letter| format!("{letter}:\\{driver}\\w11\\ARM64"))
        })
        .collect()
}

/// PowerShell for FirstLogonCommands: OpenSSH Server from the seed, or from
/// Windows Update if that did not install it. A command line holds 1024
/// characters, so setting it up is [`openssh_setup`]'s.
fn openssh_install() -> String {
    let script = [
        format!(
            "$msi = Get-PSDrive -PSProvider FileSystem | ForEach-Object {{ Join-Path $_.Root '{OPENSSH_MSI}' }} | Where-Object {{ Test-Path $_ }} | Select-Object -First 1"
        ),
        "if ($msi) { Start-Process msiexec -Wait -ArgumentList '/i',$msi,'/qn','ADDLOCAL=Server' }".to_owned(),
        "if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) { Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Add-WindowsCapability -Online }".to_owned(),
    ]
    .join("; ");
    format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"{script}\"")
}

/// The service, the key for administrators, PowerShell as the shell. The
/// service starts only in [`SSHD_START`], after every other step.
fn openssh_setup(authorized_key: &str) -> String {
    let keys = "C:\\ProgramData\\ssh\\administrators_authorized_keys";
    let script = [
        "Set-Service sshd -StartupType Automatic".to_owned(),
        "New-NetFirewallRule -Name veynode-sshd -DisplayName 'OpenSSH (VeynodeVM)' -Protocol TCP -LocalPort 22 -Action Allow".to_owned(),
        format!("Set-Content -Encoding ascii {keys} '{}'", authorized_key.trim()),
        // SIDs: the group names are translated in other Windows languages.
        format!("icacls {keys} /inheritance:r /grant *S-1-5-32-544:F /grant *S-1-5-18:F"),
        // The key may not exist yet; New-Item -Force would empty one that does.
        "if (-not (Test-Path HKLM:\\SOFTWARE\\OpenSSH)) { New-Item HKLM:\\SOFTWARE\\OpenSSH | Out-Null }"
            .to_owned(),
        "New-ItemProperty -Path HKLM:\\SOFTWARE\\OpenSSH -Name DefaultShell -PropertyType String -Force \
         -Value C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
            .to_owned(),
    ]
    .join("; ");
    format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"{script}\"")
}

/// Keeps the clock right. The guest has no clock of its own that Windows
/// reads, so each boot resumes from the time of the last shutdown, and a
/// machine in no domain leaves Windows Time stopped (a sync a week): sessions,
/// certificates and HTTPS read it. Windows Time starts with Windows, asks
/// every 15 minutes, and may step the clock however far it is off.
fn time_sync() -> String {
    let client = "HKLM:\\SYSTEM\\CurrentControlSet\\Services\\W32Time";
    let script = [
        "Set-Service w32time -StartupType Automatic".to_owned(),
        "Start-Service w32time".to_owned(),
        format!("Set-ItemProperty {client}\\TimeProviders\\NtpClient -Name SpecialPollInterval -Value 900"),
        format!("Set-ItemProperty {client}\\Config -Name MaxPosPhaseCorrection -Value 0xFFFFFFFF"),
        format!("Set-ItemProperty {client}\\Config -Name MaxNegPhaseCorrection -Value 0xFFFFFFFF"),
        "w32tm /config /manualpeerlist:'time.apple.com,0x9 time.windows.com,0x9' /syncfromflags:manual /update | Out-Null".to_owned(),
        "w32tm /resync /force | Out-Null".to_owned(),
    ]
    .join("; ");
    format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"{script}\"")
}

/// Makes [`ADMIN`] and leaves `user` a standard user. Run last, from the
/// first sign-in, which still has the administrator's rights it gives up.
fn accounts(user: &str) -> String {
    let script = [
        "$bytes = New-Object byte[] 24".to_owned(),
        "[Security.Cryptography.RNGCryptoServiceProvider]::new().GetBytes($bytes)".to_owned(),
        // Every character class, whatever the bytes were.
        "$secret = ConvertTo-SecureString ([Convert]::ToBase64String($bytes) + 'aA1!') -AsPlainText -Force".to_owned(),
        format!("New-LocalUser -Name {ADMIN} -Password $secret -PasswordNeverExpires -AccountNeverExpires | Out-Null"),
        // SIDs: the group names are translated in other Windows languages.
        format!("Add-LocalGroupMember -SID S-1-5-32-544 -Member {ADMIN}"),
        format!("reg add 'HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon\\SpecialAccounts\\UserList' /v {ADMIN} /t REG_DWORD /d 0 /f | Out-Null"),
        format!("Add-LocalGroupMember -SID S-1-5-32-545 -Member '{user}' -ErrorAction SilentlyContinue"),
        format!("Remove-LocalGroupMember -SID S-1-5-32-544 -Member '{user}'"),
    ]
    .join("; ");
    format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"{script}\"")
}

/// The last first-logon step before the power-off: once SSH answers, every
/// other step ran, so QEMU aborting in that power-off does not undo the
/// install (see [`install_state`]).
const SSHD_START: &str = "powershell -NoProfile -Command \"Start-Service sshd\"";

/// Autounattend.xml.
pub fn answer(
    user: &str,
    password: &str,
    edition: &str,
    product_key: &str,
    authorized_key: &str,
    neptune: bool,
) -> String {
    let component = |name: &str| {
        format!(
            "<component name=\"{name}\" processorArchitecture=\"arm64\" publicKeyToken=\"31bf3856ad364e35\" \
             language=\"neutral\" versionScope=\"nonSxS\">"
        )
    };
    let commands = |tag: &str, element: &str, lines: &[String]| {
        let field = if element == "SynchronousCommand" {
            "CommandLine"
        } else {
            "Path"
        };
        let body: String = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                format!(
                    "<{element} wcm:action=\"add\"><Order>{}</Order><{field}>{}</{field}></{element}>",
                    i + 1,
                    xml_text(line)
                )
            })
            .collect();
        format!("<{tag}>{body}</{tag}>")
    };
    let paths = |drivers: &[&str]| {
        let body: String = driver_paths(drivers)
            .iter()
            .enumerate()
            .map(|(i, path)| {
                format!(
                    "<PathAndCredentials wcm:action=\"add\" wcm:keyValue=\"{}\"><Path>{}</Path></PathAndCredentials>",
                    i + 1,
                    xml_text(path)
                )
            })
            .collect();
        format!("<DriverPaths>{body}</DriverPaths>")
    };
    // No TPM, no Secure Boot keys: tell Setup to skip the Windows 11 checks.
    let labconfig: Vec<String> = [
        "BypassTPMCheck",
        "BypassSecureBootCheck",
        "BypassRAMCheck",
        "BypassStorageCheck",
        "BypassCPUCheck",
    ]
    .iter()
    .map(|value| format!("reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {value} /t REG_DWORD /d 1 /f"))
    .collect();
    let letters = DRIVE_LETTERS
        .chars()
        .map(String::from)
        .collect::<Vec<_>>()
        .join(" ");
    let mut drivers = VIRTIO_DRIVERS.to_vec();
    if !neptune {
        drivers.push(DISPLAY_DRIVER);
    }
    let mut first_logon: Vec<String> = drivers
        .iter()
        // Fallback for any virtio driver offline servicing did not install.
        .map(|d| {
            format!(
                "cmd /c for %d in ({letters}) do @if exist %d:\\{d}\\w11\\ARM64 pnputil /add-driver %d:\\{d}\\w11\\ARM64\\*.inf /install"
            )
        })
        .collect();
    first_logon.extend([
        "powercfg /change monitor-timeout-ac 0".to_owned(),
        "powercfg /change standby-timeout-ac 0".to_owned(),
        // No Fast Startup: QEMU/HVF aborts (assert isv) resuming its hybrid
        // shutdown image once the Neptune GPU driver is installed.
        "powercfg /hibernate off".to_owned(),
    ]);
    if neptune {
        first_logon.push(format!(
            "cmd /c for %d in ({letters}) do @if exist %d:\\{NEPTUNE_DRIVER}\\viogpu3d.inf pnputil /add-driver %d:\\{NEPTUNE_DRIVER}\\viogpu3d.inf /install"
        ));
    }
    first_logon.extend([
        openssh_install(),
        openssh_setup(authorized_key),
        time_sync(),
        accounts(user),
        SSHD_START.to_owned(),
        // Powering off tells the host the install is complete.
        "shutdown /s /t 20 /c \"VeynodeVM install complete\"".to_owned(),
    ]);
    // Local account without a network: no "connect to the internet" page.
    let specialize = vec![
        "reg add HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\OOBE /v BypassNRO /t REG_DWORD /d 1 /f"
            .to_owned(),
    ];
    let (user, password) = (xml_text(user), xml_text(password));
    let (edition, product_key) = (xml_text(edition), xml_text(product_key));
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
<settings pass="windowsPE">
{}
<SetupUILanguage><UILanguage>en-US</UILanguage></SetupUILanguage>
<InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale>
</component>
{}{}</component>
{}
{}
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
<InstallFrom><MetaData wcm:action="add"><Key>/IMAGE/NAME</Key><Value>{edition}</Value></MetaData></InstallFrom>
<InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
</OSImage></ImageInstall>
<UserData><ProductKey><Key>{product_key}</Key><WillShowUI>Never</WillShowUI></ProductKey><AcceptEula>true</AcceptEula></UserData>
</component>
</settings>
<settings pass="offlineServicing">
{}{}</component>
</settings>
<settings pass="specialize">
{}<ComputerName>WIN-VEYNODE</ComputerName><TimeZone>UTC</TimeZone></component>
{}
{}
</component>
</settings>
<settings pass="oobeSystem">
{}<InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale></component>
{}
<OOBE><HideEULAPage>true</HideEULAPage><HideOEMRegistrationScreen>true</HideOEMRegistrationScreen><HideOnlineAccountScreens>true</HideOnlineAccountScreens><HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE><ProtectYourPC>3</ProtectYourPC></OOBE>
<UserAccounts><LocalAccounts><LocalAccount wcm:action="add"><Name>{user}</Name><DisplayName>VeynodeVM</DisplayName><Group>Administrators</Group>
<Password><Value>{password}</Value><PlainText>true</PlainText></Password></LocalAccount></LocalAccounts></UserAccounts>
<AutoLogon><Enabled>true</Enabled><Username>{user}</Username><LogonCount>9999999</LogonCount><Password><Value>{password}</Value><PlainText>true</PlainText></Password></AutoLogon>
{}
</component>
</settings>
</unattend>
"#,
        component("Microsoft-Windows-International-Core-WinPE"),
        component("Microsoft-Windows-PnpCustomizationsWinPE"),
        paths(&["viostor", "vioscsi", "NetKVM"]),
        component("Microsoft-Windows-Setup"),
        commands("RunSynchronous", "RunSynchronousCommand", &labconfig),
        component("Microsoft-Windows-PnpCustomizationsNonWinPE"),
        paths(&drivers),
        component("Microsoft-Windows-Shell-Setup"),
        component("Microsoft-Windows-Deployment"),
        commands("RunSynchronous", "RunSynchronousCommand", &specialize),
        component("Microsoft-Windows-International-Core"),
        component("Microsoft-Windows-Shell-Setup"),
        commands("FirstLogonCommands", "SynchronousCommand", &first_logon),
    )
}

/// An ISO 9660 + Joliet image labelled `label` holding `files`.
fn make_seed(work: &Path, label: &str, files: &[(&str, &[u8])]) -> Result<PathBuf> {
    let source = work.join(format!("{}-files", label.to_lowercase()));
    if source.exists() {
        fs::remove_dir_all(&source)?;
    }
    fs::create_dir_all(&source)?;
    for (name, body) in files {
        fs::write(source.join(name), body)?;
    }
    let image = work.join(format!("{}.iso", label.to_lowercase()));
    if image.exists() {
        fs::remove_file(&image)?;
    }
    let status = Command::new("/usr/bin/hdiutil")
        .args([
            "makehybrid",
            "-quiet",
            "-iso",
            "-joliet",
            "-default-volume-name",
            label,
            "-o",
        ])
        .arg(&image)
        .arg(&source)
        .status()
        .context("running hdiutil")?;
    if !status.success() {
        bail!("hdiutil makehybrid failed ({status})")
    }
    fs::remove_dir_all(&source)?;
    fs::set_permissions(&image, fs::Permissions::from_mode(0o600))?;
    Ok(image)
}

/// The machine's QEMU monitor (QMP), on its private socket.
struct Qmp {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Qmp {
    fn connect(name: &str) -> Result<Self> {
        let status = call("status", json!({"id":name}))?;
        let path = status["runtime"]["monitor_socket"]
            .as_str()
            .with_context(|| format!("{name} has no QEMU monitor"))?;
        let writer = UnixStream::connect(path).context("connecting to the QEMU monitor")?;
        writer.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut qmp = Self {
            reader: BufReader::new(writer.try_clone()?),
            writer,
        };
        qmp.read()?;
        qmp.call("qmp_capabilities", json!({}))?;
        Ok(qmp)
    }

    /// The next reply, skipping events.
    fn read(&mut self) -> Result<Value> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("the QEMU monitor closed")
            }
            let message: Value = serde_json::from_str(&line)?;
            if message.get("event").is_none() {
                return Ok(message);
            }
        }
    }

    fn call(&mut self, command: &str, arguments: Value) -> Result<Value> {
        let mut line = serde_json::to_vec(&json!({"execute":command,"arguments":arguments}))?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        let reply = self.read()?;
        if !reply["error"].is_null() {
            bail!("QMP {command}: {}", reply["error"])
        }
        Ok(reply["return"].clone())
    }

    fn key(&mut self, qcode: &str) -> Result<()> {
        self.call(
            "send-key",
            json!({"keys":[{"type":"qcode","data":qcode}],"hold-time":60}),
        )?;
        Ok(())
    }
}

fn call(method: &str, args: Value) -> Result<Value> {
    crate::call(&home::state_dir()?, method, args)
}

/// The machine's daemon status; `None` once it is not registered.
fn status(name: &str) -> Option<Value> {
    call("status", json!({"id":name})).ok()
}

/// More reboots than an install makes: Setup restarts three or four times
/// and the first logon none, so twice that is Setup starting over.
const MAX_GUEST_RESETS: u64 = 8;

/// Where the install stands by the machine's `status`: `Ok(true)` once the
/// Guest powered itself off, `Ok(false)` while it runs, else why it failed.
/// Only the Guest's own power-off ends an install: a host stop, a removal,
/// a crash or a Windows panic (QEMU exits with a failure) do not. The one
/// crash that does is QEMU's HVF `isv` abort in the final power-off once the
/// Neptune driver is in: `finished` says SSH answered, so only that
/// power-off was left. The first start after it still waits for the login.
fn install_state(name: &str, status: Option<&Value>, finished: bool) -> Result<bool> {
    let Some(status) = status else {
        bail!("{name} was removed during the install")
    };
    let runtime = &status["runtime"];
    let resets = runtime["guest_resets"].as_u64().unwrap_or(0);
    if resets > MAX_GUEST_RESETS {
        bail!("Windows Setup restarted {resets} times without finishing. Check that the ISO is Windows 11 for ARM64")
    }
    match status["state"].as_str().unwrap_or_default() {
        "stopped" if runtime["reason"] == "guest-shutdown" => Ok(true),
        "stopped" => bail!("{name} was stopped before Windows Setup finished"),
        "crashed" if finished => Ok(true),
        "crashed" => bail!("{name} crashed during the install"),
        "paused" => match runtime["qmp_status"].as_str().unwrap_or_default() {
            "prelaunch" => Ok(false),
            "io-error" => {
                bail!("{name} could not write its disk: the Mac's disk is full. Free space and create it again")
            }
            other => bail!("{name} stopped running during the install ({other})"),
        },
        "starting" | "running" | "stopping" | "unavailable" => Ok(false),
        other => bail!("{name} is {other} during the install"),
    }
}

/// Stops the machine at once and waits until the daemon says it stopped.
fn power_off(name: &str) -> Result<()> {
    call("force_stop", json!({"id":name,"confirm":name}))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let state = status(name).map(|s| s["state"].as_str().unwrap_or_default().to_owned());
        if matches!(state.as_deref(), Some("stopped" | "crashed")) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("{name} did not stop to boot the installer again")
}

/// New bytes of the machine's serial log, from where it stood at `new`.
struct Serial {
    path: PathBuf,
    offset: u64,
}

impl Serial {
    fn new(name: &str) -> Result<Self> {
        let status = call("status", json!({"id":name}))?;
        let path = PathBuf::from(
            status["config"]["serial_log"]
                .as_str()
                .with_context(|| format!("{name} has no serial log"))?,
        );
        let offset = fs::metadata(&path).map_or(0, |m| m.len());
        Ok(Self { path, offset })
    }

    fn read(&self) -> String {
        let mut text = Vec::new();
        if let Ok(mut file) = fs::File::open(&self.path) {
            if file.seek(SeekFrom::Start(self.offset)).is_ok() {
                let _ = file.read_to_end(&mut text);
            }
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    fn wait_for(&self, needle: &str, timeout: Duration, name: &str, check: Check) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.read().contains(needle) {
                return Ok(());
            }
            check()?;
            if install_state(name, status(name).as_ref(), false)? {
                bail!("{name} stopped while waiting for {needle:?}")
            }
            thread::sleep(Duration::from_secs(1));
        }
        bail!("timed out waiting for {needle:?} on the serial console")
    }
}

/// Called between polls: an error ends the install (the client went away,
/// or the machine is no longer this install's).
pub type Check<'a> = &'a dyn Fn() -> Result<()>;

/// Windows' CD boot loader waits a few seconds for a key, else falls
/// through to the next boot entry. Its prompt is drawn only on the screen,
/// so tap Enter from the moment edk2 starts the first boot entry (the CD).
/// Stop soon: a late Enter lands on Setup's focused Cancel button.
fn press_any_key(name: &str, serial: &Serial, check: Check) -> Result<()> {
    progress("waiting for edk2 to start the installer CD (serial console)");
    serial.wait_for(
        "BdsDxe: starting Boot",
        Duration::from_secs(300),
        name,
        check,
    )?;
    progress("pressing Enter for \"Press any key to boot from CD or DVD\"");
    let mut qmp = Qmp::connect(name)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        qmp.key("ret")?;
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// What the hang check compares: every vCPU's program counter (HMP `info
/// registers -a`) and the bytes written to every drive (`query-blockstats`).
#[derive(Debug, PartialEq)]
struct Activity {
    pcs: Vec<String>,
    written: u64,
}

fn activity(name: &str) -> Result<Activity> {
    let mut qmp = Qmp::connect(name)?;
    let registers = qmp.call(
        "human-monitor-command",
        json!({"command-line":"info registers -a"}),
    )?;
    let pcs = registers
        .as_str()
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once("PC="))
        .filter_map(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
        .collect();
    let written = qmp
        .call("query-blockstats", json!({}))?
        .as_array()
        .map_or(0, |drives| {
            drives
                .iter()
                .filter_map(|drive| drive["stats"]["wr_bytes"].as_u64())
                .sum()
        });
    Ok(Activity { pcs, written })
}

/// Whether the Guest's SSH server greets on the machine's forward.
fn ssh_answers(status: &Value) -> bool {
    let Some(port) = super::machine_guest::ssh_forward(&status["config"]) else {
        return false;
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], port).into(),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut banner = [0; 4];
    stream.read_exact(&mut banner).is_ok() && &banner == b"SSH-"
}

/// Less than Setup writes before its first reboot: below it, nothing was
/// installed (the firmware's variables are written in any case).
const NOTHING_WRITTEN: u64 = 64 << 20;

/// Boots of the installer before an install that never starts fails.
const INSTALLER_BOOTS: u32 = 3;

/// How a wait for the install ended without an error.
#[derive(Debug, PartialEq)]
enum Waited {
    Installed,
    /// Setup sat still before its first reboot with nothing written: it
    /// started before the seed CD's USB device showed up in Windows PE,
    /// missed Autounattend.xml and waits on its language page for a click.
    AnswerMissed,
}

/// Whether a machine that looks hung is Setup that never began: its first
/// QEMU process (no reboot yet) and nearly nothing written.
fn never_began(status: Option<&Value>, written: u64) -> bool {
    status.is_some_and(|s| s["runtime"]["guest_resets"].as_u64().unwrap_or(0) == 0)
        && written < NOTHING_WRITTEN
}

/// Waits for the Guest to power off, ending early when neither a vCPU
/// program counter nor the bytes written to disk move across two samples
/// two minutes apart (a firmware or kernel spin, as seen with a corrupt UEFI
/// variable store; an idle vCPU waits at one PC, but Setup keeps writing).
fn wait_installed(name: &str, timeout: Duration, check: Check) -> Result<Waited> {
    let deadline = Instant::now() + timeout;
    let mut next_sample = Instant::now();
    let (mut last, mut still) = (None, 0);
    let started = Instant::now();
    let mut finished = false;
    while Instant::now() < deadline {
        check()?;
        let status = status(name);
        if !finished && status.as_ref().is_some_and(ssh_answers) {
            finished = true;
            progress("the first logon is done; Windows powers off");
        }
        if install_state(name, status.as_ref(), finished)? {
            return Ok(Waited::Installed);
        }
        if Instant::now() >= next_sample {
            next_sample = Instant::now() + Duration::from_secs(120);
            // The monitor is briefly unavailable across a reset.
            let now = activity(name).ok().filter(|a| !a.pcs.is_empty());
            still = if now.is_some() && now == last {
                still + 1
            } else {
                0
            };
            last = now;
            if still >= 2 && never_began(status.as_ref(), last.as_ref().map_or(0, |a| a.written)) {
                return Ok(Waited::AnswerMissed);
            }
            if still >= 2 {
                bail!(
                    "{name} looks hung: vCPU program counters {:?} and disk writes unchanged for {} minutes",
                    last.map(|a| a.pcs).unwrap_or_default(),
                    still * 2
                )
            }
            progress(&format!(
                "Windows Setup: {}, {} minutes",
                status
                    .as_ref()
                    .map_or("", |s| s["state"].as_str().unwrap_or_default()),
                started.elapsed().as_secs() / 60
            ));
        }
        thread::sleep(Duration::from_secs(10));
    }
    bail!(
        "{name} did not finish installing within {} minutes",
        timeout.as_secs() / 60
    )
}

/// Up to `count` last lines of `bytes`, printable and at most 200
/// characters each.
fn last_lines(bytes: &[u8], count: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|line| !line.is_empty())
        .collect();
    lines.drain(..lines.len().saturating_sub(count));
    lines
}

fn file_tail(path: &Path) -> Vec<String> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let length = file.metadata().map_or(0, |m| m.len());
    let mut bytes = Vec::new();
    if file
        .seek(SeekFrom::Start(length.saturating_sub(16 << 10)))
        .is_err()
        || file.read_to_end(&mut bytes).is_err()
    {
        return Vec::new();
    }
    last_lines(&bytes, 6)
}

/// The newest QEMU stderr log of the machine (`<name>-<pid>.stderr.log`,
/// one per QEMU process) in the daemon's state directory.
fn newest_stderr_log(name: &str) -> Option<PathBuf> {
    fs::read_dir(home::state_dir().ok()?)
        .ok()?
        .flatten()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|file| {
                file.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .and_then(|rest| rest.strip_suffix(".stderr.log"))
                    .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())
        .map(|entry| entry.path())
}

/// `error` with the last lines QEMU and the serial console wrote: a failed
/// create removes both logs.
fn with_logs(error: anyhow::Error, name: &str, serial: &Path) -> anyhow::Error {
    let mut message = format!("{error:#}");
    for (label, lines) in [
        (
            "QEMU",
            newest_stderr_log(name)
                .map(|p| file_tail(&p))
                .unwrap_or_default(),
        ),
        ("serial console", file_tail(serial)),
    ] {
        if !lines.is_empty() {
            message.push_str(&format!("\n{label}:\n  {}", lines.join("\n  ")));
        }
    }
    anyhow::anyhow!(message)
}

/// Keeps the Mac from idle sleep while it lives: `caffeinate -i`, which also
/// ends with the daemon.
struct Awake(Option<std::process::Child>);

impl Awake {
    fn new() -> Self {
        let child = Command::new("/usr/bin/caffeinate")
            .args(["-i", "-w"])
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = &child {
            crate::log_line(&format!(
                "windows install: caffeinate did not start: {error}"
            ));
        }
        Self(child.ok())
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The install's key pair and answer-file image, in `work` (private), for
/// `machine.create` to attach before the machine is registered.
pub struct Prepared {
    pub key: PathBuf,
    pub seed: PathBuf,
}

pub fn prepare(name: &str, work: &Path, neptune: bool) -> Result<Prepared> {
    let work = home::private_dir(work)?;
    let key = work.join("id_ed25519");
    if !key.exists() {
        let status = Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(format!("veynodevm-{name}"))
            .arg("-f")
            .arg(&key)
            .stdin(std::process::Stdio::null())
            .status()
            .context("running ssh-keygen")?;
        if !status.success() {
            bail!("ssh-keygen failed ({status})")
        }
    }
    let authorized = fs::read_to_string(key.with_extension("pub"))?;
    let xml = answer(USER, PASSWORD, EDITION, PRODUCT_KEY, &authorized, neptune);
    // Without the installer the first logon falls back to Windows Update.
    let msi = match openssh().and_then(|path| Ok(fs::read(path)?)) {
        Ok(msi) => Some(msi),
        Err(error) => {
            crate::log_line(&format!(
                "windows install: no OpenSSH installer on the seed, Windows Update installs it: {error:#}"
            ));
            None
        }
    };
    let mut files: Vec<(&str, &[u8])> = vec![("Autounattend.xml", xml.as_bytes())];
    if let Some(msi) = &msi {
        files.push((OPENSSH_MSI, msi));
    }
    let seed = make_seed(&work, "VEYNODEWIN", &files)?;
    Ok(Prepared { key, seed })
}

/// Installs a registered machine from its ISO with the prepared answer
/// file: Setup runs unattended and the first logon powers it off. The
/// machine is left stopped. `check` runs between polls and ends the install
/// with its error. The Mac does not idle-sleep meanwhile.
pub fn install(name: &str, check: Check) -> Result<()> {
    let _awake = Awake::new();
    let started = Instant::now();
    for boot in 1..=INSTALLER_BOOTS {
        let serial = Serial::new(name)?;
        progress("starting the Windows installer");
        call("start", json!({"id":name}))?;
        let waited = press_any_key(name, &serial, check).and_then(|()| {
            progress("Windows Setup runs unattended; the first logon powers the machine off");
            wait_installed(name, Duration::from_secs(180 * 60), check)
        });
        match waited.map_err(|error| with_logs(error, name, &serial.path))? {
            Waited::Installed => break,
            Waited::AnswerMissed if boot < INSTALLER_BOOTS => {
                progress("Windows Setup missed its answer file; booting the installer again");
                power_off(name)?;
            }
            Waited::AnswerMissed => bail!(
                "Windows Setup did not find its answer file in {INSTALLER_BOOTS} boots of the installer"
            ),
        }
    }
    progress(&format!(
        "installed in {} minutes",
        started.elapsed().as_secs() / 60
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_file_installs_unattended_with_ssh_and_no_fast_startup() {
        let xml = answer(
            "a<b",
            "p&w",
            EDITION,
            PRODUCT_KEY,
            "ssh-ed25519 AAAA key\n",
            false,
        );
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<unattend "));
        assert!(xml.contains("<Name>a&lt;b</Name>") && xml.contains("<Value>p&amp;w</Value>"));
        assert!(xml.contains("<Key>/IMAGE/NAME</Key><Value>Windows 11 Pro</Value>"));
        assert!(xml.contains("/v BypassTPMCheck /t REG_DWORD"));
        assert!(xml.contains("powercfg /hibernate off"));
        assert!(xml.contains("Set-Content -Encoding ascii C:\\ProgramData\\ssh\\administrators_authorized_keys 'ssh-ed25519 AAAA key'"));
        // Group SIDs, not names a translated Windows does not have.
        assert!(xml.contains("/grant *S-1-5-32-544:F /grant *S-1-5-18:F"));
        assert!(!xml.contains("Administrators:F"));
        // The DefaultShell key exists before its value is set.
        assert!(
            xml.find("New-Item HKLM:\\SOFTWARE\\OpenSSH").unwrap()
                < xml.find("New-ItemProperty").unwrap()
        );
        // Windows PE gets the storage and network drivers on every CD letter.
        assert!(xml.contains("wcm:keyValue=\"24\"><Path>K:\\NetKVM\\w11\\ARM64</Path>"));
        assert!(!xml.contains("wcm:keyValue=\"25\"><Path>D:\\viostor"));
        // The desktop user ends a standard user; the host's SSH login is a
        // hidden administrator with a password nobody keeps.
        assert!(xml.contains("New-LocalUser -Name veynode-admin -Password $secret"));
        assert!(xml.contains("Add-LocalGroupMember -SID S-1-5-32-544 -Member veynode-admin"));
        assert!(xml.contains("SpecialAccounts\\UserList' /v veynode-admin"));
        assert!(xml.contains("Remove-LocalGroupMember -SID S-1-5-32-544 -Member 'a&lt;b'"));
        assert!(
            xml.find("administrators_authorized_keys").unwrap()
                < xml.find("Remove-LocalGroupMember").unwrap()
        );
        assert!(!xml.contains("veynode-admin</Name>"));
        assert!(accounts(USER).len() <= 1024);
        // The clock is kept by Windows Time from the first boot on.
        assert!(xml.contains("Set-Service w32time -StartupType Automatic"));
        assert!(xml.contains("SpecialPollInterval -Value 900"));
        assert!(time_sync().len() <= 1024);
        // The last first-logon command powers the machine off.
        let last = xml.rfind("<Order>").unwrap();
        assert!(xml[last..].contains("shutdown /s /t 20"));
        assert!(!xml.contains("viogpu3d"));
        // Without Neptune the display-only driver is injected, with a
        // first-logon fallback.
        assert!(xml.contains("<Path>D:\\viogpudo\\w11\\ARM64</Path>"));
        assert!(xml.contains("@if exist %d:\\viogpudo\\w11\\ARM64 pnputil /add-driver"));
        // Installed from the seed, with Windows Update only as the fallback.
        assert!(xml.find("OpenSSH.msi").unwrap() < xml.find("Add-WindowsCapability").unwrap());
        // SSH starts last, so its answer means the other steps ran.
        let start = xml.find("Start-Service sshd").unwrap();
        assert!(xml.find("Add-WindowsCapability").unwrap() < start);
        assert!(xml.find("Remove-LocalGroupMember").unwrap() < start);
        assert!(start < xml.find("shutdown /s").unwrap());
        assert_eq!(xml.matches("Start-Service sshd").count(), 1);
    }

    #[test]
    fn only_the_guest_s_own_power_off_ends_an_install() {
        let state = |state: &str, runtime: Value| {
            install_state("w", Some(&json!({"state":state,"runtime":runtime})), false)
        };
        let failure = |result: Result<bool>| result.unwrap_err().to_string();
        assert!(state("stopped", json!({"reason":"guest-shutdown"})).unwrap());
        assert!(!state("running", json!({"guest_resets":3})).unwrap());
        assert!(!state("starting", json!({"reason":"guest-reset"})).unwrap());
        assert!(!state("paused", json!({"qmp_status":"prelaunch"})).unwrap());
        for runtime in [
            json!({"reason":"host-powerdown"}),
            json!({"reason":"force_stopped"}),
            json!({"reason":"supervisor_disconnected_or_stop"}),
            json!({}),
        ] {
            assert!(failure(state("stopped", runtime)).contains("stopped before"));
        }
        assert!(failure(state("crashed", json!({}))).contains("crashed"));
        assert!(failure(state("paused", json!({"qmp_status":"io-error"}))).contains("disk is full"));
        assert!(
            failure(state("paused", json!({"qmp_status":"paused"}))).contains("stopped running")
        );
        assert!(failure(state("missing", json!({}))).contains("missing"));
        assert!(failure(install_state("w", None, false)).contains("removed"));
        // A crash once SSH answered is the power-off after the last step.
        assert!(install_state("w", Some(&json!({"state":"crashed","runtime":{}})), true).unwrap());
        assert!(!state("running", json!({"guest_resets":MAX_GUEST_RESETS})).unwrap());
        assert!(failure(state(
            "running",
            json!({"guest_resets":MAX_GUEST_RESETS + 1})
        ))
        .contains("restarted 9 times"));
    }

    #[test]
    fn only_an_ssh_greeting_says_the_first_logon_finished() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status =
            json!({"config":{"ports":[{"host_port":port,"guest_port":22,"protocol":"tcp"}]}});
        let greet = |banner: &'static [u8]| {
            let listener = listener.try_clone().unwrap();
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.write_all(banner).unwrap();
            })
        };
        let server = greet(b"SSH-2.0-OpenSSH_for_Windows\r\n");
        assert!(ssh_answers(&status));
        server.join().unwrap();
        // A forward with nothing behind it closes at once.
        let server = greet(b"");
        assert!(!ssh_answers(&status));
        server.join().unwrap();
        assert!(!ssh_answers(&json!({"config":{"ports":[]}})));
    }

    #[test]
    fn only_setup_still_before_its_first_reboot_never_began() {
        let status = |resets: u64| json!({"runtime":{"guest_resets":resets}});
        assert!(never_began(Some(&status(0)), 128 << 10));
        assert!(!never_began(Some(&status(0)), 2 << 30));
        assert!(!never_began(Some(&status(1)), 0));
        assert!(!never_began(None, 0));
    }

    #[test]
    fn an_error_keeps_the_last_printable_lines_of_a_log() {
        let log = b"one\n\x1b[2Jtwo\r\n\n  three  \nfour\n";
        assert_eq!(last_lines(log, 3), ["[2Jtwo", "three", "four"]);
        assert_eq!(last_lines(b"", 3), Vec::<String>::new());
        assert_eq!(last_lines(&[b'x'; 500], 1)[0].len(), 200);
    }

    #[test]
    fn every_first_logon_command_fits_a_command_line() {
        let key = format!(
            "ssh-ed25519 {} veynodevm-{}",
            "A".repeat(68),
            "n".repeat(70)
        );
        assert!(openssh_install().len() <= 1024 && openssh_setup(&key).len() <= 1024);
    }

    #[test]
    fn neptune_installs_its_driver_after_fast_startup_is_off() {
        let xml = answer("u", "p", EDITION, PRODUCT_KEY, "k", true);
        let driver = xml
            .find("pnputil /add-driver %d:\\Drivers\\viogpu3d\\w11\\ARM64\\viogpu3d.inf /install")
            .unwrap();
        assert!(xml.find("powercfg /hibernate off").unwrap() < driver);
        assert!(driver < xml.find("administrators_authorized_keys").unwrap());
        assert!(!xml.contains("viogpudo"));
    }
}
