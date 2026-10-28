//! The unattended Windows 11 install behind `machine.create` with
//! `unattended`, in the daemon (it was `scripts/install-distro.py windows`).
//!
//! Windows Setup reads Autounattend.xml from a seed CD-ROM labelled
//! VEYNODEWIN. TPM, Secure Boot, RAM and storage checks are bypassed
//! (LabConfig) because the machine has no TPM and no Secure Boot keys. The
//! virtio-win ARM64 drivers load in Windows PE, so Setup sees the virtio-blk
//! disk, and are injected into the installed system. The first logon turns
//! Fast Startup off, installs Windows' OpenSSH Server with this machine's
//! key for the administrator, and powers the machine off: that ends the
//! install, and `machine.create` ends with the machine stopped.
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

/// The login the answer file creates.
pub const USER: &str = "veynode";
const PASSWORD: &str = "veynode";
const EDITION: &str = "Windows 11 Pro";
/// Microsoft's generic key for the edition: it installs, it does not activate.
const PRODUCT_KEY: &str = "VK7JG-NPHTM-C97JM-9MPGT-3V66T";

/// virtio-win ARM64 driver directories (virtio-win 0.1.302 layout). viogpudo
/// is left out (virtio-ramfb stays on Basic Display), as are
/// viosock/viofs/viomem (no vhost-vsock, virtiofsd or virtio-mem on this host).
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
/// UTM's guest tools 0.1.273, the release whose viogpu3d speaks Neptune wire
/// format 1, as UTM 5.0.5's runtime does (0.1.274 speaks 2 and fails;
/// docs/known-issues.md). Downloaded from UTM's QEMU release, not shipped.
pub const GUEST_TOOLS: veynodevm_oci::download::Pinned = veynodevm_oci::download::Pinned {
    url:
        "https://github.com/utmapp/qemu/releases/download/v10.0.12-utm/utm-guest-tools-0.1.273.iso",
    size: 140_195_840,
    sha256: "7d2c0343e92358ad5e65078b08ec1dad873eb91b3c154aef60531bf6c2f04601",
};
/// The Neptune driver's directory on the guest tools CD-ROM.
const NEPTUNE_DRIVER: &str = "Drivers\\viogpu3d\\w11\\ARM64";

/// [`GUEST_TOOLS`] in the home's download cache, fetched the first time.
pub fn guest_tools() -> Result<PathBuf> {
    let path =
        home::private_dir(&home::root()?.join("downloads"))?.join("utm-guest-tools-0.1.273.iso");
    veynodevm_oci::download::fetch(&GUEST_TOOLS, &path, &mut |m| progress(m))?;
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

/// PowerShell for FirstLogonCommands: Windows' OpenSSH Server (a Windows
/// Update download), the key for administrators, PowerShell as the shell.
fn openssh_server(authorized_key: &str) -> String {
    let keys = "C:\\ProgramData\\ssh\\administrators_authorized_keys";
    let script = [
        "Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Add-WindowsCapability -Online".to_owned(),
        "Set-Service sshd -StartupType Automatic".to_owned(),
        "Start-Service sshd".to_owned(),
        "New-NetFirewallRule -Name veynode-sshd -DisplayName 'OpenSSH (VeynodeVM)' -Protocol TCP -LocalPort 22 -Action Allow".to_owned(),
        format!("Set-Content -Encoding ascii {keys} '{}'", authorized_key.trim()),
        format!("icacls {keys} /inheritance:r /grant Administrators:F /grant SYSTEM:F"),
        "New-ItemProperty -Path HKLM:\\SOFTWARE\\OpenSSH -Name DefaultShell -PropertyType String -Force \
         -Value C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
            .to_owned(),
    ]
    .join("; ");
    format!("powershell -NoProfile -ExecutionPolicy Bypass -Command \"{script}\"")
}

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
    let mut first_logon: Vec<String> = VIRTIO_DRIVERS
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
        openssh_server(authorized_key),
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
        paths(&VIRTIO_DRIVERS),
        component("Microsoft-Windows-Shell-Setup"),
        component("Microsoft-Windows-Deployment"),
        commands("RunSynchronous", "RunSynchronousCommand", &specialize),
        component("Microsoft-Windows-International-Core"),
        component("Microsoft-Windows-Shell-Setup"),
        commands("FirstLogonCommands", "SynchronousCommand", &first_logon),
    )
}

/// An ISO 9660 + Joliet image labelled `label` holding `files`.
fn make_seed(work: &Path, label: &str, files: &[(&str, &str)]) -> Result<PathBuf> {
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

fn vm_state(name: &str) -> String {
    call("status", json!({"id":name}))
        .ok()
        .and_then(|v| v["state"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "missing".into())
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

    fn wait_for(&self, needle: &str, timeout: Duration, name: &str) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.read().contains(needle) {
                return Ok(());
            }
            if matches!(vm_state(name).as_str(), "stopped" | "crashed") {
                bail!("{name} stopped while waiting for {needle:?}")
            }
            thread::sleep(Duration::from_secs(1));
        }
        bail!("timed out waiting for {needle:?} on the serial console")
    }
}

/// Windows' CD boot loader waits a few seconds for a key, else falls
/// through to the next boot entry. Its prompt is drawn only on the screen,
/// so tap Enter from the moment edk2 starts the first boot entry (the CD).
/// Stop soon: a late Enter lands on Setup's focused Cancel button.
fn press_any_key(name: &str, serial: &Serial) -> Result<()> {
    progress("waiting for edk2 to start the installer CD (serial console)");
    serial.wait_for("BdsDxe: starting Boot", Duration::from_secs(300), name)?;
    progress("pressing Enter for \"Press any key to boot from CD or DVD\"");
    let mut qmp = Qmp::connect(name)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        qmp.key("ret")?;
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// Program counters of every vCPU (HMP `info registers -a`).
fn vcpu_pcs(name: &str) -> Result<Vec<String>> {
    let out = Qmp::connect(name)?.call(
        "human-monitor-command",
        json!({"command-line":"info registers -a"}),
    )?;
    Ok(out
        .as_str()
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once("PC="))
        .filter_map(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
        .collect())
}

/// Waits for the Guest to power off, ending early when no vCPU program
/// counter moves across two samples two minutes apart (a firmware or kernel
/// spin, as seen with a corrupt UEFI variable store).
fn wait_installed(name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut next_sample = Instant::now();
    let (mut pcs, mut still) = (None, 0);
    let started = Instant::now();
    while Instant::now() < deadline {
        let state = vm_state(name);
        match state.as_str() {
            "stopped" => return Ok(()),
            "crashed" => bail!("the Windows installer VM crashed"),
            _ => {}
        }
        if Instant::now() >= next_sample {
            next_sample = Instant::now() + Duration::from_secs(120);
            // The monitor is briefly unavailable across a reset.
            let now = vcpu_pcs(name).ok().filter(|p| !p.is_empty());
            still = if now.is_some() && now == pcs {
                still + 1
            } else {
                0
            };
            pcs = now;
            if still >= 2 {
                bail!(
                    "{name} looks hung: vCPU program counters {:?} unchanged for {} minutes",
                    pcs.unwrap_or_default(),
                    still * 2
                )
            }
            progress(&format!(
                "Windows Setup: {state}, {} minutes",
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
    let seed = make_seed(&work, "VEYNODEWIN", &[("Autounattend.xml", &xml)])?;
    Ok(Prepared { key, seed })
}

/// Installs a registered machine from its ISO with the prepared answer
/// file: Setup runs unattended and the first logon powers it off. The
/// machine is left stopped. A failure leaves it for inspection.
pub fn install(name: &str) -> Result<()> {
    let serial = Serial::new(name)?;
    let started = Instant::now();
    progress("starting the Windows installer");
    call("start", json!({"id":name}))?;
    press_any_key(name, &serial)?;
    progress("Windows Setup runs unattended; the first logon powers the machine off");
    wait_installed(name, Duration::from_secs(180 * 60))?;
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
        // Windows PE gets the storage and network drivers on every CD letter.
        assert!(xml.contains("wcm:keyValue=\"24\"><Path>K:\\NetKVM\\w11\\ARM64</Path>"));
        assert!(!xml.contains("wcm:keyValue=\"25\"><Path>D:\\viostor"));
        // The last first-logon command powers the machine off.
        let last = xml.rfind("<Order>").unwrap();
        assert!(xml[last..].contains("shutdown /s /t 20"));
        assert!(!xml.contains("viogpu3d"));
    }

    #[test]
    fn neptune_installs_its_driver_after_fast_startup_is_off() {
        let xml = answer("u", "p", EDITION, PRODUCT_KEY, "k", true);
        let driver = xml
            .find("pnputil /add-driver %d:\\Drivers\\viogpu3d\\w11\\ARM64\\viogpu3d.inf /install")
            .unwrap();
        assert!(xml.find("powercfg /hibernate off").unwrap() < driver);
        assert!(driver < xml.find("administrators_authorized_keys").unwrap());
    }
}
