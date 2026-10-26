//! QEMU arguments for container-oriented VM options: user-network port
//! forwards, read-only extra disks and 9p host shares.
use crate::Config;
use anyhow::{bail, Result};

/// `-nic` value; port forwards become slirp `hostfwd` rules.
pub fn nic(config: &Config) -> String {
    let mut nic = String::from("user,model=virtio-net-pci");
    // Validated as a locally administered unicast address (no commas).
    if let Some(mac) = &config.mac_address {
        nic.push_str(&format!(",mac={mac}"));
    }
    for port in &config.ports {
        nic.push_str(&format!(
            ",hostfwd={}:{}:{}-:{}",
            port.protocol, port.host_address, port.host_port, port.guest_port
        ));
    }
    nic
}

fn option_value(value: &str) -> Result<&str> {
    // QEMU option strings use ',' as a separator; refuse rather than escape.
    if value.contains(',') || value.contains('\0') {
        bail!("QEMU option paths must not contain commas")
    }
    Ok(value)
}

/// Arguments attached after the primary disk so Guest device order is
/// primary disk, then extra disks in configuration order.
pub fn arguments(config: &Config) -> Result<Vec<String>> {
    let mut args = Vec::new();
    for (index, disk) in config.extra_disks.iter().enumerate() {
        let path = option_value(disk.path.to_str().unwrap_or(","))?;
        args.push("-blockdev".into());
        args.push(serde_json::to_string(&serde_json::json!({
            "driver":"file","node-name":format!("veynode-extra{index}-file"),
            "filename":path,"read-only":true,"locking":"off"
        }))?);
        args.push("-blockdev".into());
        args.push(serde_json::to_string(&serde_json::json!({
            "driver":"raw","node-name":format!("veynode-extra{index}"),
            "file":format!("veynode-extra{index}-file"),"read-only":true
        }))?);
        args.extend(crate::qemu_uefi::extra_disk_devices(config, index));
    }
    for share in &config.shares {
        let path = option_value(share.path.to_str().unwrap_or(","))?;
        // Writable shares keep guest ownership/mode in xattrs, so a guest chown
        // (docker creating a missing bind source) succeeds without changing
        // real host ownership; QEMU runs unprivileged and could not chown.
        let model = if share.read_only {
            "none"
        } else {
            "mapped-xattr"
        };
        let mut value = format!(
            "local,path={path},mount_tag={},security_model={model},id=veynode-share-{}",
            share.tag, share.tag
        );
        if share.read_only {
            value.push_str(",readonly=on");
        }
        args.push("-virtfs".into());
        args.push(value);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(extra: serde_json::Value) -> Config {
        let mut base = serde_json::json!({
            "backend":"qemu-hvf","id":"c1","guest":"linux","cpus":1,"memory_mib":512,
            "kernel":"/k"
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }
    #[test]
    fn port_forwards_become_hostfwd_rules() {
        let c = config(serde_json::json!({"ports":[
            {"host_port":8080,"guest_port":80},
            {"host_address":"0.0.0.0","host_port":5353,"guest_port":53,"protocol":"udp"}
        ]}));
        assert_eq!(
            nic(&c),
            "user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:8080-:80,hostfwd=udp:0.0.0.0:5353-:53"
        );
    }
    #[test]
    fn fixed_mac_address_is_passed_to_the_nic() {
        let c = config(serde_json::json!({"mac_address":"02:11:22:33:44:55"}));
        assert_eq!(nic(&c), "user,model=virtio-net-pci,mac=02:11:22:33:44:55");
    }
    #[test]
    fn extra_disks_and_shares_are_ordered_and_read_only() {
        let c = config(serde_json::json!({
            "extra_disks":[{"path":"/img/base.sqfs","read_only":true}],
            "shares":[{"tag":"v0","path":"/Users/me/data","read_only":true}]
        }));
        let args = arguments(&c).unwrap();
        assert!(args[1].contains("\"read-only\":true") && args[1].contains("/img/base.sqfs"));
        assert_eq!(
            args[5],
            "virtio-blk-pci,drive=veynode-extra0,id=veynode-extra0-device"
        );
        assert_eq!(
            args[7],
            "local,path=/Users/me/data,mount_tag=v0,security_model=none,id=veynode-share-v0,readonly=on"
        );
    }
    #[test]
    fn commas_in_paths_are_refused() {
        let c = config(serde_json::json!({"shares":[{"tag":"v0","path":"/a,b"}]}));
        assert!(arguments(&c).is_err());
    }
}
