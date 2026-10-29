//! QEMU arguments for UEFI-booted full distributions: edk2 code in read-only
//! pflash0, a per-VM 64 MiB variable store in pflash1, a read-only installer
//! CD-ROM on virtio-scsi and USB keyboard/tablet (edk2 has USB HID drivers but
//! no virtio-input driver, so bootloader menus need USB input).
//!
//! Windows ARM guests: the installer CD-ROM and seed images are USB CD-ROMs
//! (usb-bot + scsi-cd: Windows Setup has inbox drivers for them and reads
//! ISO 9660/UDF only from CD-ROM devices, and the answer file must be readable
//! before any virtio driver is loaded), the display is UTM's virtio-ramfb, and
//! the disk, NIC, serial, balloon, RNG and input are virtio devices whose
//! ARM64 drivers come from the virtio-win ISO. `qemu_disk_bus: "nvme"` keeps
//! a disk installed without viostor bootable.
use crate::{BootMode, Config};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
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
/// A balloon that is never inflated; with free page reporting the Guest hands back
/// the memory it frees, which the loader returns to macOS.
pub const BALLOON_DEVICE: &str = "virtio-balloon-pci,id=veynode-balloon,free-page-reporting=on";

/// The flash contents of a variable template. UTM ships `edk2-arm-vars.fd` as
/// a qcow2 image (QEMU auto-detects the format); copying its bytes verbatim
/// gives edk2 qcow2 metadata instead of an erased (0xFF) variable/FTW area,
/// which it only partly reformats. Raw templates are used as they are.
pub fn template_flash(bytes: &[u8]) -> Result<Vec<u8>> {
    if !bytes.starts_with(b"QFI\xfb") {
        return Ok(bytes.to_vec());
    }
    let be32 = |at: usize| -> Result<u64> {
        Ok(u32::from_be_bytes(
            bytes
                .get(at..at + 4)
                .context("truncated qcow2")?
                .try_into()?,
        ) as u64)
    };
    let be64 = |at: usize| -> Result<u64> {
        Ok(u64::from_be_bytes(
            bytes
                .get(at..at + 8)
                .context("truncated qcow2")?
                .try_into()?,
        ))
    };
    let (version, backing, bits, size) = (be32(4)?, be64(8)?, be32(20)?, be64(24)?);
    let (encrypted, l1_entries, l1_offset) = (be32(32)?, be32(36)?, be64(40)?);
    ensure!(
        matches!(version, 2 | 3) && backing == 0 && encrypted == 0 && (9..=21).contains(&bits),
        "unsupported qcow2 UEFI variable template"
    );
    ensure!(
        size <= FLASH_BYTES,
        "UEFI variable template larger than 64 MiB"
    );
    if version == 3 {
        // Incompatible features (dirty, corrupt, external data, compression type) are unsupported.
        ensure!(
            be64(72)? == 0,
            "unsupported qcow2 UEFI variable template features"
        );
    }
    let cluster = 1u64 << bits;
    let per_l2 = cluster / 8;
    let mut flash = vec![0u8; size as usize];
    for i in 0..l1_entries {
        let l2 = be64(l1_offset as usize + 8 * i as usize)? & 0x00ff_ffff_ffff_fe00;
        if l2 == 0 {
            continue;
        }
        for j in 0..per_l2 {
            let entry = be64((l2 + 8 * j) as usize)?;
            let at = (i * per_l2 + j) * cluster;
            if entry == 0 || at >= size {
                continue;
            }
            let end = (at + cluster).min(size) as usize;
            let target = &mut flash[at as usize..end];
            if entry & (1 << 62) != 0 {
                let offset_bits = 62 - (bits - 8);
                let offset = (entry & ((1 << offset_bits) - 1)) as usize;
                let sectors = ((entry >> offset_bits) & ((1 << (bits - 8)) - 1)) + 1;
                let stored = bytes
                    .get(offset..(offset + sectors as usize * 512).min(bytes.len()))
                    .context("truncated qcow2 compressed cluster")?;
                let mut inflate = flate2::read::DeflateDecoder::new(stored);
                inflate
                    .read_exact(target)
                    .context("invalid qcow2 compressed cluster")?;
            } else if entry & 1 == 0 {
                let offset = (entry & 0x00ff_ffff_ffff_fe00) as usize;
                target.copy_from_slice(
                    bytes
                        .get(offset..offset + target.len())
                        .context("truncated qcow2 cluster")?,
                );
            } // else: an all-zero cluster
        }
    }
    // Unallocated clusters read as zero, as QEMU would present them.
    while flash.last() == Some(&0) {
        flash.pop();
    }
    ensure!(!flash.is_empty(), "UEFI variable template is empty");
    Ok(flash)
}

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
            file.write_all(&template_flash(&body)?)?;
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
    if let Some(cdrom) = &config.cdrom {
        args.extend(blockdev("veynode-cdrom", cdrom, true)?);
        let controller = if windows(config) {
            "usb-bot,bus=veynode-xhci.0,id=veynode-scsi"
        } else {
            "virtio-scsi-pci,id=veynode-scsi"
        };
        let mut cdrom =
            String::from("scsi-cd,bus=veynode-scsi.0,drive=veynode-cdrom,id=veynode-cdrom-device");
        if boot_order(config) {
            // The installer first while it is attached, then the disk.
            cdrom.push_str(",bootindex=0");
        }
        args.extend(["-device".into(), controller.into(), "-device".into(), cdrom]);
    }
    if windows(config) {
        // virtio-win ARM64: vioinput, Balloon, viorng, pvpanic. edk2 has no
        // virtio-input driver, so the USB keyboard/tablet above stay.
        args.extend(
            [
                "-device",
                "virtio-keyboard-pci,id=veynode-virtio-keyboard",
                "-device",
                "virtio-tablet-pci,id=veynode-virtio-tablet",
                "-device",
                BALLOON_DEVICE,
                "-device",
                "virtio-rng-pci,id=veynode-rng",
                "-device",
                "pvpanic-pci,id=veynode-pvpanic",
            ]
            .map(String::from),
        );
    }
    if let Some(path) = monitor {
        let path = path.to_str().context("monitor path must be UTF-8")?;
        ensure!(!path.contains(','), "monitor path must not contain commas");
        args.extend(["-qmp".into(), format!("unix:{path},server=on,wait=off")]);
    }
    Ok(args)
}

pub fn windows(config: &Config) -> bool {
    config.guest == "windows"
}

/// A Linux machine's disk and installer CD-ROM carry `bootindex`, so the
/// firmware boots the disk wherever it sits on the PCI bus: with QEMU's boot
/// order, edk2 puts the matching boot options first and drops stale ones (an
/// earlier layout's slot, PXE) that would otherwise come before a new entry
/// for the disk and end at the UEFI shell.
fn boot_order(config: &Config) -> bool {
    config.boot == BootMode::Uefi && !windows(config)
}

/// `-device` arguments that go after every other device. QEMU gives PCI
/// devices slots in command-line order and a machine's firmware variables name
/// its disk by slot, so a device added in a later release goes here, never
/// before the disk: a Linux machine keeps the layout it was installed with
/// (disk at 00:05.0, its seed after it).
pub fn trailing_devices(config: &Config) -> Vec<String> {
    if boot_order(config) {
        // Distribution kernels load virtio_balloon for it, as container-init does.
        return ["-device", BALLOON_DEVICE].map(String::from).to_vec();
    }
    Vec::new()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskBus {
    #[default]
    Virtio,
    Nvme,
}
impl DiskBus {
    pub fn is_default(&self) -> bool {
        *self == Self::Virtio
    }
}

/// The iothread the writable virtio-blk disk runs on, off the main loop
/// (`qemu_disk::DiskGuard::configure_command` creates it).
pub const DISK_IOTHREAD: &str = "veynode-disk-io";

/// `-device` for the primary disk (drive `veynode-disk`). QEMU 10.0's `nvme`
/// has no `iothread` property, so it stays on the main loop. A Linux
/// machine's disk boots first ([`boot_order`]).
pub fn disk_device(config: &Config) -> &'static str {
    match (config.qemu_disk_bus, boot_order(config)) {
        (DiskBus::Virtio, true) => {
            "virtio-blk-pci,drive=veynode-disk,iothread=veynode-disk-io,id=veynode-disk-device,bootindex=1"
        }
        (DiskBus::Virtio, false) => {
            "virtio-blk-pci,drive=veynode-disk,iothread=veynode-disk-io,id=veynode-disk-device"
        }
        (DiskBus::Nvme, _) => "nvme,drive=veynode-disk,serial=veynode-disk,id=veynode-disk-device",
    }
}

/// `-device` arguments for read-only extra disk `index`.
pub fn extra_disk_devices(config: &Config, index: usize) -> Vec<String> {
    if windows(config) {
        vec![
            "-device".into(),
            format!("usb-bot,bus=veynode-xhci.0,id=veynode-extra{index}-bot"),
            "-device".into(),
            format!("scsi-cd,bus=veynode-extra{index}-bot.0,drive=veynode-extra{index},id=veynode-extra{index}-device"),
        ]
    } else {
        vec![
            "-device".into(),
            format!("virtio-blk-pci,drive=veynode-extra{index},id=veynode-extra{index}-device"),
        ]
    }
}

/// `-device` for the display. Windows: UTM's `virtio-ramfb`, a PCI virtio-gpu
/// that also provides ramfb, which the UEFI framebuffer and the Windows basic
/// display driver use (plain `ramfb` is not a PCI device, so SPICE refuses it).
pub fn display_device(config: &Config) -> Result<String> {
    if windows(config) {
        // UTM 5.0.8's default Windows GPU (`UTMQemuConfiguration+Arguments.swift`):
        // the same device with Venus and Neptune on.
        return Ok(if config.qemu_graphics_profile.neptune() {
            "virtio-ramfb-gl,hostmem=8G,blob=on,venus=on,neptune=on".into()
        } else {
            "virtio-ramfb".into()
        });
    }
    config
        .qemu_graphics_profile
        .device(config.width, config.height)
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

    /// qcow2 v3, 512-byte clusters: raw, compressed, zero-flag and unallocated.
    fn qcow2(clusters: &[Option<(u8, bool)>]) -> Vec<u8> {
        use std::io::Write as _;
        let mut image = vec![0u8; 512 * 3];
        let put32 =
            |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_be_bytes());
        let put64 =
            |b: &mut Vec<u8>, at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_be_bytes());
        image[..4].copy_from_slice(b"QFI\xfb");
        put32(&mut image, 4, 3);
        put32(&mut image, 20, 9);
        put64(&mut image, 24, 512 * clusters.len() as u64);
        put32(&mut image, 36, 1);
        put64(&mut image, 40, 512); // L1 at cluster 1, L2 at cluster 2
        put64(&mut image, 512, 1024);
        for (j, cluster) in clusters.iter().enumerate() {
            let entry = match cluster {
                None => 0,
                Some((_, true)) if j == 2 => 1, // zero flag
                Some((byte, compressed)) => {
                    let offset = image.len() as u64;
                    if *compressed {
                        let mut deflate = flate2::write::DeflateEncoder::new(
                            Vec::new(),
                            flate2::Compression::best(),
                        );
                        deflate.write_all(&[*byte; 512]).unwrap();
                        let mut data = deflate.finish().unwrap();
                        data.resize(512, 0);
                        image.extend(data);
                        (1 << 62) | offset // one sector
                    } else {
                        image.extend([*byte; 512]);
                        offset
                    }
                }
            };
            put64(&mut image, 1024 + 8 * j, entry);
        }
        image
    }

    #[test]
    fn qcow2_templates_are_decoded_and_raw_ones_kept() {
        let image = qcow2(&[
            Some((0x11, false)),
            Some((0xff, true)),
            Some((0, true)),
            None,
            Some((0x22, true)),
            None,
        ]);
        let flash = template_flash(&image).unwrap();
        assert_eq!(flash.len(), 512 * 5);
        assert!(flash[..512].iter().all(|b| *b == 0x11));
        assert!(flash[512..1024].iter().all(|b| *b == 0xff));
        assert!(flash[1024..2048].iter().all(|b| *b == 0));
        assert!(flash[2048..].iter().all(|b| *b == 0x22));
        assert_eq!(template_flash(b"_FVH raw").unwrap(), b"_FVH raw");
        let mut backed = image.clone();
        backed[8..16].copy_from_slice(&1u64.to_be_bytes());
        assert!(template_flash(&backed).is_err());
        assert!(template_flash(&image[..600]).is_err());
        // UTM's shipped template, when the runtime is present locally.
        let utm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local/runtime/utm/UTM.app/Contents/Resources/qemu")
            .join(VARS_TEMPLATE_FILE);
        if let Ok(bytes) = fs::read(utm) {
            let flash = template_flash(&bytes).unwrap();
            assert_eq!(&flash[0x28..0x2c], b"_FVH");
            assert!(flash[0x10000..0x40000].iter().all(|b| *b == 0xff));
        }
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

    /// QEMU numbers PCI devices in command-line order and a machine's firmware
    /// variables name its disk by that slot. Machines made by 0.1.0 have the
    /// disk at 00:05.0 (NIC, display, xhci, virtio-serial before it); 0.1.1
    /// first put the balloon before the disk, moved it to 00:06.0, and edk2 then
    /// stopped at the UEFI shell. Only xhci (and an installer's virtio-scsi) may
    /// come from `arguments`; new devices are `trailing_devices`.
    #[test]
    fn a_linux_machine_keeps_the_pci_layout_its_firmware_recorded() {
        let (_temp, root) = private_dir();
        let pci = |c: &Config| -> Vec<String> {
            let args = arguments(c, Path::new("/res"), None).unwrap();
            args.windows(2)
                .filter(|w| w[0] == "-device" && !w[1].contains("bus="))
                .map(|w| w[1].clone())
                .collect()
        };
        let installed = config(&root, json!({"cdrom":null}));
        assert_eq!(pci(&installed), ["qemu-xhci,id=veynode-xhci"]);
        let installing = config(&root, json!({}));
        assert_eq!(
            pci(&installing),
            [
                "qemu-xhci,id=veynode-xhci",
                "virtio-scsi-pci,id=veynode-scsi"
            ]
        );
        let joined = arguments(&installing, Path::new("/res"), None)
            .unwrap()
            .join(" ");
        assert!(joined.contains("id=veynode-cdrom-device,bootindex=0"));
        assert!(disk_device(&installed).ends_with(",bootindex=1"));
        assert_eq!(trailing_devices(&installed), ["-device", BALLOON_DEVICE]);
        // The seed and other extra disks are never boot devices.
        assert!(!extra_disk_devices(&installed, 0)
            .concat()
            .contains("bootindex"));
    }

    #[test]
    fn windows_guests_get_usb_media_virtio_devices_and_ramfb() {
        let (_temp, root) = private_dir();
        let c = config(&root, json!({"guest":"windows"}));
        c.validate().unwrap();
        let joined = arguments(&c, Path::new("/res"), None).unwrap().join(" ");
        assert!(joined.contains("usb-bot,bus=veynode-xhci.0,id=veynode-scsi"));
        assert!(joined.contains("scsi-cd,bus=veynode-scsi.0,drive=veynode-cdrom"));
        assert!(!joined.contains("virtio-scsi-pci"));
        // The USB controller exists before the CD-ROM is plugged into it.
        assert!(joined.find("qemu-xhci").unwrap() < joined.find("usb-bot").unwrap());
        assert!(disk_device(&c).starts_with("virtio-blk-pci,drive=veynode-disk,"));
        for device in [
            "virtio-tablet-pci",
            "virtio-balloon-pci",
            "virtio-rng-pci",
            "pvpanic-pci",
        ] {
            assert!(joined.contains(device), "{device}");
        }
        assert!(joined.contains("virtio-balloon-pci,id=veynode-balloon,free-page-reporting=on"));
        let nvme = config(&root, json!({"guest":"windows","qemu_disk_bus":"nvme"}));
        nvme.validate().unwrap();
        assert!(disk_device(&nvme).starts_with("nvme,drive=veynode-disk,"));
        assert!(!disk_device(&nvme).contains("iothread"));
        assert!(config(
            &root,
            json!({"backend":"apple-virtualization","qemu_disk_bus":"nvme"})
        )
        .validate()
        .is_err());
        assert_eq!(
            extra_disk_devices(&c, 1)[1..4],
            [
                "usb-bot,bus=veynode-xhci.0,id=veynode-extra1-bot",
                "-device",
                "scsi-cd,bus=veynode-extra1-bot.0,drive=veynode-extra1,id=veynode-extra1-device"
            ]
        );
        assert_eq!(display_device(&c).unwrap(), "virtio-ramfb");
        let gpu = config(
            &root,
            json!({"guest":"windows","qemu_graphics_profile":"neptune-dxmt-experimental"}),
        );
        gpu.validate().unwrap();
        assert_eq!(
            display_device(&gpu).unwrap(),
            "virtio-ramfb-gl,hostmem=8G,blob=on,venus=on,neptune=on"
        );
        assert!(config(
            &root,
            json!({"qemu_graphics_profile":"neptune-dxmt-experimental"})
        )
        .validate()
        .is_err());
        assert!(!disk_device(&c).contains("bootindex"));
        assert!(!joined.contains("bootindex"));
        assert!(trailing_devices(&c).is_empty());
        let linux = config(&root, json!({}));
        assert_eq!(
            disk_device(&linux),
            "virtio-blk-pci,drive=veynode-disk,iothread=veynode-disk-io,id=veynode-disk-device,bootindex=1"
        );
        assert!(disk_device(&linux).contains(&format!(",iothread={DISK_IOTHREAD},")));
        assert_eq!(
            extra_disk_devices(&linux, 0),
            [
                "-device",
                "virtio-blk-pci,drive=veynode-extra0,id=veynode-extra0-device"
            ]
        );
        let joined = arguments(&linux, Path::new("/res"), None)
            .unwrap()
            .join(" ");
        assert!(!joined.contains("virtio-balloon"));
        assert_eq!(trailing_devices(&linux), ["-device", BALLOON_DEVICE]);
        assert!(!joined.contains("virtio-rng-pci"));
        assert!(display_device(&linux)
            .unwrap()
            .starts_with("virtio-gpu-gl-pci"));
        // Windows needs QEMU UEFI: no Apple Virtualization, direct boot, Venus or 9p.
        let kernel = root.join("Image");
        fs::write(&kernel, b"k").unwrap();
        let share = root.join("share");
        fs::create_dir(&share).unwrap();
        for extra in [
            json!({"guest":"windows","backend":"apple-virtualization"}),
            json!({"guest":"windows","boot":"kernel","kernel":kernel,"efi_vars":null,"cdrom":null}),
            json!({"guest":"windows","qemu_graphics_profile":"venus-moltenvk-experimental"}),
            json!({"guest":"windows","shares":[{"tag":"v0","path":share,"read_only":true}]}),
        ] {
            assert!(config(&root, extra.clone()).validate().is_err(), "{extra}");
        }
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
