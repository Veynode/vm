//! The NoCloud seed (`CIDATA` ISO 9660 image) of a Linux machine created
//! from an upstream cloud image (`machine create --disk`): its hostname, the
//! login `veynode` with passwordless sudo and the per-home SSH key, host
//! shares mounted at `/mnt/TAG`, and on Apple Virtualization the guest agent
//! as a desktop-only service (`--desktop-stream`: the display endpoint's
//! scanout and input on vsock 5001), copied from the seed itself.
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const USER: &str = "veynode";

pub struct Seed<'a> {
    pub instance_id: &'a str,
    pub hostname: &'a str,
    pub authorized_key: &'a str,
    /// (tag, read-only); mounted at `/mnt/TAG`.
    pub shares: &'a [(String, bool)],
    /// QEMU shares are 9p, Apple Virtualization ones virtiofs.
    pub qemu: bool,
    /// The Linux arm64 guest agent to install as a service, if any.
    pub agent: Option<&'a Path>,
}

/// A YAML double-quoted scalar.
fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("string")
}

pub fn hostname(name: &str) -> String {
    let host: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(63)
        .collect();
    if host.is_empty() {
        "veynodevm".into()
    } else {
        host
    }
}

impl Seed<'_> {
    pub fn meta_data(&self) -> String {
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            quoted(self.instance_id),
            quoted(self.hostname)
        )
    }

    pub fn user_data(&self) -> String {
        let mut out = String::from("#cloud-config\n");
        out += &format!("hostname: {}\n", quoted(self.hostname));
        out += "preserve_hostname: false\nssh_pwauth: false\ndisable_root: true\n";
        out += "growpart: {mode: auto, devices: ['/']}\n";
        out += &format!(
            "users:\n  - name: {USER}\n    gecos: VeynodeVM\n    shell: /bin/bash\n    lock_passwd: true\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    groups: [adm, sudo]\n    ssh_authorized_keys:\n      - {}\n",
            quoted(self.authorized_key.trim())
        );
        if !self.shares.is_empty() {
            out += "mounts:\n";
            for (tag, read_only) in self.shares {
                let options = match (self.qemu, read_only) {
                    (true, true) => "trans=virtio,version=9p2000.L,msize=262144,ro,nofail",
                    (true, false) => "trans=virtio,version=9p2000.L,msize=262144,nofail",
                    (false, true) => "ro,nofail",
                    (false, false) => "defaults,nofail",
                };
                let kind = if self.qemu { "9p" } else { "virtiofs" };
                out += &format!("  - [{tag}, /mnt/{tag}, {kind}, \"{options}\", \"0\", \"0\"]\n");
            }
        }
        if self.agent.is_some() {
            out += "write_files:\n  - path: /etc/systemd/system/veynode-agent.service\n    permissions: '0644'\n    content: |\n";
            for line in [
                "[Unit]",
                "Description=VeynodeVM guest agent (display and input)",
                "After=systemd-udev-settle.service",
                "[Service]",
                "ExecStart=/usr/local/bin/veynode-agent --desktop-stream",
                "Restart=always",
                "RestartSec=2",
                "[Install]",
                "WantedBy=multi-user.target",
            ] {
                out += &format!("      {line}\n");
            }
        }
        out += "runcmd:\n";
        for (tag, _) in self.shares {
            out += &format!("  - [sh, -c, \"mkdir -p /mnt/{tag} && (mountpoint -q /mnt/{tag} || mount /mnt/{tag}) || true\"]\n");
        }
        if self.agent.is_some() {
            out += "  - [sh, -c, \"mkdir -p /run/veynode-seed && mount -o ro LABEL=cidata /run/veynode-seed 2>/dev/null || mount -o ro LABEL=CIDATA /run/veynode-seed; install -m 0755 /run/veynode-seed/veynode-agent /usr/local/bin/veynode-agent; umount /run/veynode-seed; systemctl daemon-reload; systemctl enable --now veynode-agent.service\"]\n";
        }
        // The host waits for this line on the serial console before exec.
        out += "  - [sh, -c, \"echo VEYNODE-CLOUD-INIT-DONE > /dev/console || true\"]\n";
        out
    }

    /// Writes the seed image to `out` (replacing it), through `hdiutil`.
    pub fn write(&self, work: &Path, out: &Path) -> Result<()> {
        let files = work.join("cidata");
        fs::create_dir_all(&files)?;
        fs::write(files.join("meta-data"), self.meta_data())?;
        fs::write(files.join("user-data"), self.user_data())?;
        fs::write(files.join("network-config"), NETWORK)?;
        if let Some(agent) = self.agent {
            fs::copy(agent, files.join("veynode-agent"))
                .with_context(|| format!("cannot copy the guest agent {}", agent.display()))?;
        }
        let _ = fs::remove_file(out);
        let output = Command::new("/usr/bin/hdiutil")
            .args([
                "makehybrid",
                "-quiet",
                "-iso",
                "-joliet",
                "-default-volume-name",
                "cidata",
                "-o",
            ])
            .arg(out)
            .arg(&files)
            .stdin(Stdio::null())
            .output()
            .context("running hdiutil")?;
        let _ = fs::remove_dir_all(&files);
        if !output.status.success() {
            bail!(
                "hdiutil makehybrid failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        Ok(())
    }
}

/// DHCP on the first Ethernet NIC, whatever the image calls it.
/// The MAC as DHCP client id, so the host finds the lease by the machine's MAC.
const NETWORK: &str = "version: 2\nethernets:\n  primary:\n    match: {name: \"e*\"}\n    dhcp4: true\n    dhcp6: false\n    dhcp-identifier: mac\n";

/// The Linux arm64 guest agent shipped with this program: next to it, in
/// the app bundle's `Resources/guest`, or a checkout's `.local/bin` build.
pub fn agent() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    let shipped = [
        dir.join("guest/veynode-agent"),
        dir.join("veynode-agent"),
        dir.join("../Resources/guest/veynode-agent"),
        dir.join("../share/guest/veynode-agent"),
    ];
    let checkout = exe
        .ancestors()
        .find(|d| d.join("crates/cli/Cargo.toml").is_file())
        .map(|root| root.join(".local/bin/veynode-guest"));
    shipped.into_iter().chain(checkout).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(qemu: bool, agent: Option<&Path>) -> String {
        Seed {
            instance_id: "abc123",
            hostname: &hostname("My Box_1"),
            authorized_key: "ssh-ed25519 AAAA veynodevm\n",
            shares: &[("src".into(), false), ("data".into(), true)],
            qemu,
            agent,
        }
        .user_data()
    }

    #[test]
    fn user_data_installs_the_key_and_mounts_shares() {
        assert_eq!(hostname("My Box_1"), "my-box-1");
        let apple = seed(false, Some(Path::new("/a")));
        assert!(apple.starts_with("#cloud-config\n"));
        assert!(apple.contains("      - \"ssh-ed25519 AAAA veynodevm\"\n"));
        assert!(apple.contains("NOPASSWD:ALL"));
        assert!(apple.contains("[src, /mnt/src, virtiofs, \"defaults,nofail\""));
        assert!(apple.contains("[data, /mnt/data, virtiofs, \"ro,nofail\""));
        assert!(apple.contains("veynode-agent --desktop-stream"));
        let qemu = seed(true, None);
        assert!(qemu.contains(
            "[data, /mnt/data, 9p, \"trans=virtio,version=9p2000.L,msize=262144,ro,nofail\""
        ));
        assert!(!qemu.contains("veynode-agent"));
        let meta = Seed {
            instance_id: "i\"d",
            hostname: "h",
            authorized_key: "",
            shares: &[],
            qemu: false,
            agent: None,
        }
        .meta_data();
        assert_eq!(meta, "instance-id: \"i\\\"d\"\nlocal-hostname: \"h\"\n");
    }
}
