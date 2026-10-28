//! A macOS machine's port relay: the daemon and its worker reach the
//! Guest's own loopback services (sshd for `machine.exec` and `files.copy`,
//! Screen Sharing for `machine.display`, published ports) over virtio-vsock
//! instead of the Guest's vmnet address, which macOS's Local Network privacy
//! keeps from a launchd-started daemon until the person allows it.
//!
//! `machine.create` writes the relay (`native/guest-relay/relay.c`) and a
//! LaunchAgent for it into the image login's home on the cloned disk, before
//! the first boot: the disk image is attached without mounting and its Data
//! volume mounted by this user with ownership on, so the files are the
//! user's, which is the Guest login's own when the uids match (501 in Tart
//! images). No root, no network, nothing for the person to approve; Tart
//! images log their login in, so the agent runs from the first boot. A
//! machine without it (another uid, no relay in the runtime, an older
//! machine) keeps the vmnet address.
//!
//! In the Guest the relay listens on vsock port 5022 for the host only; each
//! connection opens with the Guest TCP port as one line. The worker listens
//! on the machine's `guest_ports_socket` and splices each connection to the
//! relay (`native/worker/GuestPorts.swift`).
use crate::home;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const LABEL: &str = "dev.veynode.relay";
/// Where the relay lives in the login's home.
const INSTALLED: &str = "Library/Application Support/VeynodeVM/veynode-relay";

/// The socket of a machine's relay: short, since a Unix socket path is
/// limited to 104 bytes, and in a directory private to this user, which
/// the worker creates again after a reboot empties /private/tmp.
pub fn socket(id: &str) -> PathBuf {
    PathBuf::from(format!("/private/tmp/veynodevm-{}/{id}.sock", unsafe {
        libc::getuid()
    }))
}

/// The relay socket of a VM configuration, if it has one.
pub fn of(config: &Value) -> Option<PathBuf> {
    config["guest_ports_socket"].as_str().map(PathBuf::from)
}

/// A stream to the Guest's TCP `port` through the relay at `socket`.
pub fn connect(socket: &Path, port: u16) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(format!("{port}\n").as_bytes())?;
    Ok(stream)
}

/// Whether the Guest's sshd answers through the relay: its banner arrives.
pub fn ssh_answers(socket: &Path) -> bool {
    let Ok(mut stream) = connect(socket, 22) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut banner = [0u8; 4];
    stream.read_exact(&mut banner).is_ok() && &banner == b"SSH-"
}

/// The `ssh -o ProxyCommand=` that reaches the Guest's sshd through `socket`.
pub fn proxy_command(socket: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("the daemon's executable")?;
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    // ssh expands % tokens in the command.
    Ok(format!("{} __guest_port {} 22", quote(&exe), quote(socket)).replace('%', "%%"))
}

/// `vm-control __guest_port SOCKET PORT`: stdin and stdout spliced to the
/// Guest's TCP `port` through the relay, for ssh's ProxyCommand.
pub fn proxy(socket: &str, port: &str) -> Result<()> {
    let port: u16 = port.parse().context("invalid port")?;
    let stream =
        connect(Path::new(socket), port).with_context(|| format!("the Guest relay at {socket}"))?;
    let mut inward = stream.try_clone()?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut inward);
        let _ = inward.shutdown(Shutdown::Write);
    });
    let mut outward = stream;
    io::copy(&mut outward, &mut io::stdout().lock())?;
    Ok(())
}

/// The relay's LaunchAgent, running `program` in the login's session.
fn agent(program: &Path) -> String {
    let escaped = program
        .display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{escaped}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// Writes the relay and its agent into `user`'s home on the mounted Data
/// volume `root`. `Ok(false)`: that home is not this user's uid.
fn write_into(root: &Path, user: &str, relay: &[u8]) -> Result<bool> {
    let home = root.join("Users").join(user);
    let meta = fs::metadata(&home)
        .with_context(|| format!("the image has no home for its login {user}"))?;
    if meta.uid() != unsafe { libc::getuid() } {
        return Ok(false);
    }
    let program = home.join(INSTALLED);
    let agents = home.join("Library/LaunchAgents");
    for dir in [program.parent().unwrap(), agents.as_path()] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)?;
    }
    let write = |path: &Path, bytes: &[u8], mode: u32| -> Result<()> {
        let _ = fs::remove_file(path);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .and_then(|mut f| f.write_all(bytes))
            .with_context(|| format!("writing {}", path.display()))
    };
    write(&program, relay, 0o755)?;
    write(
        &agents.join(format!("{LABEL}.plist")),
        agent(&Path::new("/Users").join(user).join(INSTALLED)).as_bytes(),
        0o644,
    )?;
    Ok(true)
}

fn plist_json(output: &[u8]) -> Result<Value> {
    let mut child = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(output)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("plutil could not read a plist")
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

fn run(command: &mut Command) -> Result<Vec<u8>> {
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        bail!(
            "{:?}: {}",
            command.get_program(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(output.stdout)
}

/// The Data volume (`diskNsM`) of the APFS container on `whole` (`diskN`).
fn data_volume(whole: &str) -> Result<String> {
    let list = plist_json(&run(
        Command::new("/usr/sbin/diskutil").args(["apfs", "list", "-plist"])
    )?)?;
    let prefix = format!("{whole}s");
    for container in list["Containers"].as_array().into_iter().flatten() {
        let ours = container["PhysicalStores"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| {
                s["DeviceIdentifier"]
                    .as_str()
                    .is_some_and(|d| d.starts_with(&prefix))
            });
        if !ours {
            continue;
        }
        for volume in container["Volumes"].as_array().into_iter().flatten() {
            let data = volume["Roles"]
                .as_array()
                .is_some_and(|roles| roles.iter().any(|r| r == "Data"));
            if let (true, Some(id)) = (data, volume["DeviceIdentifier"].as_str()) {
                return Ok(id.to_owned());
            }
        }
    }
    bail!("the disk has no APFS Data volume")
}

/// Writes the runtime's relay into the macOS disk `disk` (raw, not running)
/// for its login `user`, mounting under `work`. `Ok(false)` when the runtime
/// has no relay or the login's home is not this user's uid, so the machine
/// keeps the vmnet address.
pub fn install(disk: &Path, user: &str, work: &Path) -> Result<bool> {
    let Some(relay) = home::Runtime::load()?.macos_relay else {
        return Ok(false);
    };
    let relay = fs::read(&relay).with_context(|| format!("reading {}", relay.display()))?;
    install_bytes(disk, user, work, &relay)
}

fn install_bytes(disk: &Path, user: &str, work: &Path, relay: &[u8]) -> Result<bool> {
    if user.is_empty()
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Ok(false);
    }
    let attached = plist_json(&run(Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nomount",
            "-noverify",
            "-plist",
        ])
        .arg(disk))?)?;
    let whole = attached["system-entities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["dev-entry"].as_str())
        .filter_map(|d| d.strip_prefix("/dev/"))
        .find(|d| d.strip_prefix("disk").is_some_and(|n| !n.contains('s')))
        .context("hdiutil attached no disk")?
        .to_owned();
    let mount = work.join("mnt");
    let result = (|| -> Result<bool> {
        let volume = data_volume(&whole)?;
        home::private_dir(&mount)?;
        run(Command::new("/sbin/mount")
            .args(["-t", "apfs", "-o", "nobrowse,owners"])
            .arg(format!("/dev/{volume}"))
            .arg(&mount))?;
        let written = write_into(&mount, user, relay);
        let unmounted = run(Command::new("/sbin/umount").arg(&mount));
        let written = written?;
        unmounted?;
        Ok(written)
    })();
    let detached = run(Command::new("/usr/bin/hdiutil").args(["detach", &whole]));
    if detached.is_err() {
        let _ = run(Command::new("/usr/bin/hdiutil").args(["detach", "-force", &whole]));
    }
    let _ = fs::remove_dir(&mount);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_lands_in_the_logins_home() {
        let root = std::env::temp_dir().join(format!("relay-{}", std::process::id()));
        let home = root.join("Users/admin");
        fs::create_dir_all(home.join("Library")).unwrap();
        assert!(write_into(&root, "admin", b"relay").unwrap());
        assert_eq!(fs::read(home.join(INSTALLED)).unwrap(), b"relay");
        let plist =
            fs::read_to_string(home.join("Library/LaunchAgents/dev.veynode.relay.plist")).unwrap();
        assert!(plist.contains(
            "<string>/Users/admin/Library/Application Support/VeynodeVM/veynode-relay</string>"
        ));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(write_into(&root, "nobody-here", b"relay").is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_proxy_names_the_socket_and_escapes_percent() {
        let command = proxy_command(Path::new("/private/tmp/veynodevm-501/a%b.sock")).unwrap();
        assert!(command.ends_with("__guest_port '/private/tmp/veynodevm-501/a%%b.sock' 22"));
        assert!(socket("abc").as_os_str().len() < 104);
    }

    #[test]
    fn connections_open_with_the_port_line() {
        let dir = std::env::temp_dir().join(format!("relay-sock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut line = [0u8; 3];
            s.read_exact(&mut line).unwrap();
            assert_eq!(&line, b"22\n");
            s.write_all(b"SSH-2.0-test\r\n").unwrap();
        });
        assert!(ssh_answers(&path));
        server.join().unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Detaches an attached image when the test ends, passing or not.
    struct Detach(String);
    impl Drop for Detach {
        fn drop(&mut self) {
            let _ = run(Command::new("/usr/bin/hdiutil").args(["detach", "-force", &self.0]));
        }
    }

    fn attach(disk: &Path) -> String {
        let attached = plist_json(
            &run(Command::new("/usr/bin/hdiutil")
                .args([
                    "attach",
                    "-imagekey",
                    "diskimage-class=CRawDiskImage",
                    "-nomount",
                    "-plist",
                ])
                .arg(disk))
            .unwrap(),
        )
        .unwrap();
        attached["system-entities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["dev-entry"].as_str()?.strip_prefix("/dev/"))
            .find(|d| d.strip_prefix("disk").is_some_and(|n| !n.contains('s')))
            .unwrap()
            .to_owned()
    }

    /// The whole disk path on an APFS image with a Data volume, as a Tart
    /// disk has: attach, find the volume, mount as this user, write, detach.
    /// Uses `hdiutil` and `diskutil`; run with `--ignored`.
    #[test]
    #[ignore]
    fn install_writes_into_an_apfs_data_volume() {
        let dir = std::env::temp_dir().join(format!("relay-disk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        home::private_dir(&dir).unwrap();
        let dmg = dir.join("t.dmg");
        run(Command::new("/usr/bin/hdiutil")
            .args([
                "create", "-size", "128m", "-fs", "APFS", "-volname", "Sys", "-layout", "GPTSPUD",
            ])
            .arg(&dmg))
        .unwrap();
        run(Command::new("/usr/bin/hdiutil")
            .args(["convert", "-format", "UDTO", "-o"])
            .arg(dir.join("t"))
            .arg(&dmg))
        .unwrap();
        let disk = dir.join("disk.img");
        fs::rename(dir.join("t.cdr"), &disk).unwrap();
        // A Data-role volume holding the login's home.
        let whole = attach(&disk);
        let _detach = Detach(whole.clone());
        let list = plist_json(
            &run(Command::new("/usr/sbin/diskutil").args(["apfs", "list", "-plist"])).unwrap(),
        )
        .unwrap();
        let first = list["Containers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| {
                c["PhysicalStores"][0]["DeviceIdentifier"]
                    .as_str()
                    .is_some_and(|d| d.starts_with(&format!("{whole}s")))
            })
            .unwrap()["Volumes"][0]["DeviceIdentifier"]
            .as_str()
            .unwrap()
            .to_owned();
        run(Command::new("/usr/sbin/diskutil").args(["apfs", "changeVolumeRole", &first, "D"]))
            .unwrap();
        let mount = dir.join("m");
        home::private_dir(&mount).unwrap();
        let volume = data_volume(&whole).unwrap();
        run(Command::new("/sbin/mount")
            .args(["-t", "apfs", "-o", "nobrowse,owners"])
            .arg(format!("/dev/{volume}"))
            .arg(&mount))
        .unwrap();
        fs::create_dir_all(mount.join("Users/admin/Library")).unwrap();
        run(Command::new("/sbin/umount").arg(&mount)).unwrap();
        drop(_detach);

        let installed = install_bytes(&disk, "admin", &dir, b"relay");

        let whole = attach(&disk);
        let _detach = Detach(whole.clone());
        let volume = data_volume(&whole).unwrap();
        home::private_dir(&mount).unwrap();
        run(Command::new("/sbin/mount")
            .args(["-t", "apfs", "-o", "nobrowse,owners,rdonly"])
            .arg(format!("/dev/{volume}"))
            .arg(&mount))
        .unwrap();
        let program = mount.join("Users/admin").join(INSTALLED);
        let bytes = fs::read(&program).ok();
        let meta = fs::metadata(&program)
            .map(|m| (m.uid(), m.mode() & 0o777))
            .ok();
        let plist = fs::read_to_string(
            mount.join("Users/admin/Library/LaunchAgents/dev.veynode.relay.plist"),
        )
        .ok();
        let _ = run(Command::new("/sbin/umount").arg(&mount));
        drop(_detach);
        fs::remove_dir_all(&dir).unwrap();
        assert!(installed.unwrap());
        assert_eq!(bytes.as_deref(), Some(&b"relay"[..]));
        assert_eq!(meta, Some((unsafe { libc::getuid() }, 0o755)));
        assert!(plist.unwrap().contains(LABEL));
    }
}
