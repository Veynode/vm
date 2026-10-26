//! QEMU arguments for UEFI-booted full distributions: edk2 code in read-only
//! pflash0, a per-VM 64 MiB variable store in pflash1, a read-only installer
//! CD-ROM on virtio-scsi and USB keyboard/tablet (edk2 has USB HID drivers but
//! no virtio-input driver, so bootloader menus need USB input).
use crate::{BootMode, Config};
use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

/// QEMU `virt` requires each flash bank to be exactly 64 MiB.
pub const FLASH_BYTES: u64 = 64 << 20;
pub const CODE_FILE: &str = "edk2-aarch64-code.fd";
pub const VARS_TEMPLATE_FILE: &str = "edk2-arm-vars.fd";

/// Create `vars` from the firmware's empty variable template on first use, or
/// check that an existing store is a private 64 MiB pflash image.
pub fn prepare_vars(template: &Path, vars: &Path) -> Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(vars)
    {
        Ok(mut file) => {
            let mut body = Vec::new();
            File::open(template)
                .with_context(|| format!("UEFI variable template missing: {}", template.display()))?
                .take(FLASH_BYTES + 1)
                .read_to_end(&mut body)?;
            ensure!(
                !body.is_empty() && body.len() as u64 <= FLASH_BYTES,
                "UEFI variable template must be 1 byte through 64 MiB"
            );
            file.write_all(&body)?;
            // The remainder is unused flash; a sparse tail keeps the file small.
            file.set_len(FLASH_BYTES)?;
            file.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(vars)
                .context("efi_vars must not be a symbolic link")?;
            let m = file.metadata()?;
            ensure!(
                m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
                "efi_vars must be a private regular file owned by you"
            );
            if m.len() != FLASH_BYTES {
                bail!("efi_vars is not a QEMU UEFI variable store (expected a 64 MiB pflash image; Apple Virtualization stores are not interchangeable)")
            }
            Ok(())
        }
        Err(e) => Err(e).context("cannot create efi_vars"),
    }
}

fn blockdev(node: &str, filename: &Path, read_only: bool) -> Result<[String; 4]> {
    let filename = filename.to_str().context("path must be UTF-8")?;
    Ok([
        "-blockdev".into(),
        serde_json::to_string(&json!({
            "driver":"file","node-name":format!("{node}-file"),"filename":filename,
            "read-only":read_only
        }))?,
        "-blockdev".into(),
        serde_json::to_string(&json!({
            "driver":"raw","node-name":node,"file":format!("{node}-file"),"read-only":read_only
        }))?,
    ])
}

/// Firmware, installer medium, USB input and (optionally) monitor arguments
/// for `boot: "uefi"`. `resources` is the registered QEMU data directory.
pub fn arguments(config: &Config, resources: &Path, monitor: Option<&Path>) -> Result<Vec<String>> {
    ensure!(config.boot == BootMode::Uefi, "not a UEFI configuration");
    let vars = config.efi_vars.as_deref().context("efi_vars required")?;
    let mut args = Vec::new();
    args.extend(blockdev(
        "veynode-efi-code",
        &resources.join(CODE_FILE),
        true,
    )?);
    args.extend(blockdev("veynode-efi-vars", vars, false)?);
    args.extend([
        "-machine".into(),
        "pflash0=veynode-efi-code,pflash1=veynode-efi-vars".into(),
    ]);
    if let Some(cdrom) = &config.cdrom {
        args.extend(blockdev("veynode-cdrom", cdrom, true)?);
        args.extend([
            "-device".into(),
            "virtio-scsi-pci,id=veynode-scsi".into(),
            "-device".into(),
            "scsi-cd,bus=veynode-scsi.0,drive=veynode-cdrom,id=veynode-cdrom-device".into(),
        ]);
    }
    args.extend(
        [
            "-device",
            "qemu-xhci,id=veynode-xhci",
            "-device",
            "usb-kbd,bus=veynode-xhci.0,id=veynode-keyboard",
            "-device",
            "usb-tablet,bus=veynode-xhci.0,id=veynode-tablet",
        ]
        .map(String::from),
    );
    if let Some(path) = monitor {
        let path = path.to_str().context("monitor path must be UTF-8")?;
        ensure!(!path.contains(','), "monitor path must not contain commas");
        args.extend(["-qmp".into(), format!("unix:{path},server=on,wait=off")]);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn private_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        (temp, root)
    }
    fn config(root: &Path, extra: serde_json::Value) -> Config {
        let iso = root.join("installer.iso");
        fs::write(&iso, vec![0u8; 4096]).unwrap();
        let mut base = json!({
            "backend":"qemu-hvf","id":"m1","guest":"linux","cpus":2,"memory_mib":2048,
            "boot":"uefi","efi_vars":root.join("efi-vars.fd"),"cdrom":iso
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn vars_are_created_from_template_and_padded_to_flash_size() {
        let (_temp, root) = private_dir();
        let template = root.join("template.fd");
        fs::write(&template, b"_FVH-template").unwrap();
        let vars = root.join("vars.fd");
        prepare_vars(&template, &vars).unwrap();
        let m = fs::metadata(&vars).unwrap();
        assert_eq!(m.len(), FLASH_BYTES);
        assert_eq!(m.mode() & 0o777, 0o600);
        assert!(fs::read(&vars).unwrap().starts_with(b"_FVH-template"));
        // Reuse keeps the guest's boot entries: no overwrite.
        fs::write(&template, b"other").unwrap();
        prepare_vars(&template, &vars).unwrap();
        assert!(fs::read(&vars).unwrap().starts_with(b"_FVH-template"));
    }

    #[test]
    fn foreign_symlinked_or_shared_vars_are_refused() {
        let (_temp, root) = private_dir();
        let template = root.join("template.fd");
        fs::write(&template, b"t").unwrap();
        let small = root.join("vz.store");
        fs::write(&small, b"vz variable store").unwrap();
        fs::set_permissions(&small, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(prepare_vars(&template, &small).is_err());
        let link = root.join("link.fd");
        std::os::unix::fs::symlink(root.join("elsewhere"), &link).unwrap();
        assert!(prepare_vars(&template, &link).is_err());
        assert!(!root.join("elsewhere").exists());
        let shared = root.join("shared.fd");
        fs::write(&shared, vec![0u8; FLASH_BYTES as usize]).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(prepare_vars(&template, &shared).is_err());
    }

    #[test]
    fn uefi_arguments_use_pflash_scsi_cdrom_and_usb_input() {
        let (_temp, root) = private_dir();
        let c = config(&root, json!({}));
        c.validate().unwrap();
        let monitor = root.join("monitor.sock");
        let args = arguments(&c, Path::new("/res"), Some(&monitor)).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("\"filename\":\"/res/edk2-aarch64-code.fd\",\"node-name\":\"veynode-efi-code-file\",\"read-only\":true"));
        assert!(joined.contains("\"node-name\":\"veynode-efi-vars-file\",\"read-only\":false"));
        assert!(joined.contains("-machine pflash0=veynode-efi-code,pflash1=veynode-efi-vars"));
        assert!(joined
            .contains("installer.iso\",\"node-name\":\"veynode-cdrom-file\",\"read-only\":true"));
        assert!(joined.contains("scsi-cd,bus=veynode-scsi.0,drive=veynode-cdrom"));
        assert!(joined.contains("usb-kbd,bus=veynode-xhci.0") && joined.contains("usb-tablet"));
        assert!(joined.ends_with(&format!(
            "-qmp unix:{},server=on,wait=off",
            monitor.display()
        )));
        let without_cdrom = config(&root, json!({"cdrom":null}));
        let args = arguments(&without_cdrom, Path::new("/res"), None)
            .unwrap()
            .join(" ");
        assert!(!args.contains("scsi-cd") && !args.contains("-qmp"));
    }

    #[test]
    fn uefi_validation_is_strict() {
        let (_temp, root) = private_dir();
        let kernel = root.join("Image");
        fs::write(&kernel, b"k").unwrap();
        assert!(config(&root, json!({})).validate().is_ok());
        assert!(config(&root, json!({"qemu_monitor":true}))
            .validate()
            .is_ok());
        for (field, value) in [
            ("kernel", json!(kernel)),
            ("command_line", json!("console=ttyAMA0")),
            ("efi_vars", json!(null)),
            ("efi_vars", json!("relative/vars.fd")),
            ("efi_vars", json!(root.join("missing-dir/vars.fd"))),
            ("efi_vars", json!(root.join("a/../vars.fd"))),
            ("cdrom", json!(root.join("missing.iso"))),
            ("cdrom", json!(root)),
            ("qemu_guest_exec", json!(true)),
            ("guest", json!("macos")),
        ] {
            let mut c = config(&root, json!({}));
            let mut value_json = serde_json::to_value(&c).unwrap();
            value_json[field] = value;
            c = match serde_json::from_value(value_json) {
                Ok(c) => c,
                Err(_) => continue,
            };
            assert!(c.validate().is_err(), "{field} must be refused");
        }
        // Neither disk nor cdrom: nothing to boot.
        assert!(config(&root, json!({"cdrom":null})).validate().is_err());
        // efi_vars and cdrom without UEFI boot.
        let direct = config(&root, json!({"boot":"kernel","kernel":kernel}));
        assert!(direct.validate().is_err());
        let link = root.join("iso-link.iso");
        std::os::unix::fs::symlink(root.join("installer.iso"), &link).unwrap();
        assert!(config(&root, json!({"cdrom":link})).validate().is_err());
        let apple = config(&root, json!({"backend":"apple-virtualization"}));
        assert!(apple.validate().is_ok());
        let apple_monitor = config(
            &root,
            json!({"backend":"apple-virtualization","qemu_monitor":true}),
        );
        assert!(apple_monitor.validate().is_err());
    }

    #[test]
    fn default_config_keeps_direct_kernel_boot_and_serialization() {
        let c: Config = serde_json::from_value(json!({
            "id":"c","guest":"linux","cpus":1,"memory_mib":512,"kernel":"/k"
        }))
        .unwrap();
        assert_eq!(c.boot, BootMode::Kernel);
        let value = serde_json::to_value(&c).unwrap();
        for field in ["boot", "efi_vars", "cdrom", "qemu_monitor"] {
            assert!(value.get(field).is_none(), "{field}");
        }
        assert!(serde_json::from_value::<Config>(json!({
            "id":"c","guest":"linux","cpus":1,"memory_mib":512,"boot":"bios"
        }))
        .is_err());
    }
}
