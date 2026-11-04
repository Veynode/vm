//! DNS for QEMU guests whose user network cannot forward to the host's
//! resolvers.
//!
//! QEMU's user network (libslirp 4.9) answers the guest's 10.0.2.3 by sending
//! the query to the first IPv4 `nameserver` that `res_ninit` reads from
//! `/etc/resolv.conf`, or to the first IPv6 one with its scope id dropped.
//! On a network whose only resolver is a link-local IPv6 address (an
//! IPv6-only Wi-Fi with a router at `fe80::…%en0`) neither works: slirp drops
//! IPv4 queries and cannot reach the unscoped IPv6 address, so the guest
//! cannot resolve names although the host can. A host relay cannot help:
//! slirp keeps the destination port, so the guest could reach one only on the
//! host's port 53, which an unprivileged process can bind only on the wildcard
//! address, where it would take the queries of Apple Virtualization guests.
//!
//! So an agent-managed guest that was given no servers gets the host's own
//! resolvers that it can reach through slirp's NAT (global IPv6 ones), or,
//! when the host has none, public resolvers as the last resort.
use serde_json::{json, Value};
use std::{fs, io, net::IpAddr, path::Path};

/// The file `res_ninit` reads on macOS; it mirrors the default resolver.
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// Used only when no host resolver is reachable from the guest.
pub const PUBLIC_FALLBACK: [&str; 2] = ["1.1.1.1", "8.8.8.8"];

/// In a container's directory, marks servers this module chose, so the next
/// boot decides again instead of keeping them as if the person had passed
/// `--dns`. (The guest agent refuses unknown configuration fields.)
const AUTO_FILE: &str = "dns-auto";

/// The `nameserver` addresses in resolv.conf text, scope ids kept as text.
fn nameservers(resolv_conf: &str) -> Vec<String> {
    resolv_conf
        .lines()
        .filter_map(|line| {
            let line = line.split(['#', ';']).next()?.trim();
            let mut words = line.split_whitespace();
            if words.next()? != "nameserver" {
                return None;
            }
            words.next().map(str::to_owned)
        })
        .collect()
}

/// The address part of a nameserver entry (`fe80::1%en0` → `fe80::1`).
fn address(server: &str) -> Option<(IpAddr, bool)> {
    let (addr, scope) = match server.split_once('%') {
        Some((addr, _)) => (addr, true),
        None => (server, false),
    };
    addr.parse().ok().map(|ip| (ip, scope))
}

/// Whether slirp forwards the guest's 10.0.2.3 queries with this resolver
/// configuration: it needs an IPv4 server (a loopback one works too, slirp
/// sends from the host).
pub fn slirp_forwards(resolv_conf: &str) -> bool {
    nameservers(resolv_conf)
        .iter()
        .filter_map(|s| address(s))
        .any(|(ip, _)| ip.is_ipv4())
}

/// A host resolver a guest behind slirp's NAT can query directly: a global
/// IPv6 address (link-local, scoped and loopback ones exist only on the host).
fn guest_reachable(ip: IpAddr, scoped: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            let link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
            !scoped
                && !link_local
                && !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
        }
    }
}

/// The servers to give a QEMU guest that has none of its own, or `None` when
/// slirp's own forwarder works with this configuration.
pub fn servers_for(resolv_conf: &str) -> Option<Vec<String>> {
    if slirp_forwards(resolv_conf) {
        return None;
    }
    let reachable: Vec<String> = nameservers(resolv_conf)
        .iter()
        .filter_map(|s| address(s))
        .filter(|&(ip, scoped)| guest_reachable(ip, scoped))
        .map(|(ip, _)| ip.to_string())
        .take(3)
        .collect();
    Some(if reachable.is_empty() {
        PUBLIC_FALLBACK.iter().map(|s| (*s).to_owned()).collect()
    } else {
        reachable
    })
}

/// Set the DNS servers of a guest configuration for one boot. Servers this
/// module chose for an earlier boot (`chosen_before`) are dropped first;
/// servers the person set are kept. Returns whether servers were chosen for
/// this boot.
pub fn apply(
    guest: &mut Value,
    qemu: bool,
    resolv_conf: Option<&str>,
    chosen_before: bool,
) -> bool {
    if chosen_before {
        guest["dns"] = json!([]);
    }
    let own = guest["dns"].as_array().is_some_and(|d| !d.is_empty());
    if !qemu || own {
        return false;
    }
    let Some(servers) = resolv_conf.and_then(servers_for) else {
        return false;
    };
    guest["dns"] = json!(servers);
    true
}

/// [`apply`] for the container in `dir`, keeping its marker in step.
pub fn apply_in(
    dir: &Path,
    guest: &mut Value,
    qemu: bool,
    resolv_conf: Option<&str>,
) -> io::Result<bool> {
    let marker = dir.join(AUTO_FILE);
    let chosen = apply(guest, qemu, resolv_conf, marker.exists());
    if chosen {
        fs::write(&marker, b"")?;
    } else {
        clear_auto(dir)?;
    }
    Ok(chosen)
}

/// Servers the person sets replace any this module chose.
pub fn clear_auto(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join(AUTO_FILE)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A fork's configuration is its source's, chosen servers included.
pub fn copy_auto(from: &Path, to: &Path) -> io::Result<()> {
    if from.join(AUTO_FILE).exists() {
        fs::write(to.join(AUTO_FILE), b"")?;
    }
    Ok(())
}

/// The shell script (run as root) that makes a Linux machine's network use
/// `servers` until its next boot, through the first of these the guest has:
/// systemd-networkd (a runtime drop-in for each Ethernet link's network
/// file, replacing the servers DHCP and slirp's router advertisement gave),
/// NetworkManager (`nmcli device modify`, which lasts until the device
/// reconnects) or a plain `/etc/resolv.conf`. Everything it writes is under
/// `/run` or rewritten by the guest's DHCP client, so the next boot decides
/// again. It prints `set` when it changed the guest, nothing when this boot
/// already uses `servers`.
pub fn machine_script(servers: &[String]) -> String {
    let all = servers.join(" ");
    let (v4, v6): (Vec<&str>, Vec<&str>) = servers
        .iter()
        .map(String::as_str)
        .partition(|s| !s.contains(':'));
    let networkd = format!(
        "[Network]\\nDNS={all}\\n[DHCPv4]\\nUseDNS=no\\n[DHCPv6]\\nUseDNS=no\\n[IPv6AcceptRA]\\nUseDNS=no\\n"
    );
    let nm = format!(
        "ipv4.ignore-auto-dns yes ipv4.dns '{}' ipv6.ignore-auto-dns yes ipv6.dns '{}'",
        v4.join(" "),
        v6.join(" ")
    );
    format!(
        r#"m=/run/veynode-dns; S='{all}'
[ "$(cat $m 2>/dev/null)" = "$S" ] && exit 0
done=
if command -v networkctl >/dev/null && systemctl is-active -q systemd-networkd; then
  for l in $(networkctl list --no-legend 2>/dev/null | awk '$3=="ether"{{print $2}}'); do
    f=$(networkctl status "$l" 2>/dev/null | sed -n 's/^ *Network File: //p')
    case "$f" in /*) d="/run/systemd/network/${{f##*/}}.d"; mkdir -p "$d" && printf '{networkd}' > "$d/90-veynode-dns.conf" && done=1;; esac
  done
  [ -n "$done" ] && networkctl reload
fi
if [ -z "$done" ] && command -v nmcli >/dev/null && systemctl is-active -q NetworkManager; then
  for d in $(nmcli -t -f DEVICE,TYPE,STATE device 2>/dev/null | awk -F: '$2=="ethernet" && $3=="connected"{{print $1}}'); do
    nmcli device modify "$d" {nm} >/dev/null && done=1
  done
fi
if [ -z "$done" ] && [ -f /etc/resolv.conf ] && [ ! -L /etc/resolv.conf ]; then
  for s in $S; do echo "nameserver $s"; done > /etc/resolv.conf && done=1
fi
[ -n "$done" ] || exit 1
echo "$S" > $m; echo set
"#
    )
}

/// The host's resolver configuration, if it can be read.
pub fn host_resolv_conf() -> Option<String> {
    std::fs::read_to_string(RESOLV_CONF).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK_LOCAL_ONLY: &str =
        "#\n# macOS Notice\n#\nnameserver fe80::8096:98ff:fe33:9764%en0\n";

    #[test]
    fn ipv4_servers_leave_slirp_in_charge() {
        assert_eq!(servers_for("nameserver 192.168.1.1\n"), None);
        // A local proxy on loopback: slirp sends to 127.0.0.1 from the host.
        assert_eq!(servers_for("nameserver 127.0.0.1\n"), None);
        assert_eq!(
            servers_for("nameserver fe80::1%en0\nnameserver 10.0.0.1\n"),
            None
        );
    }

    #[test]
    fn link_local_only_resolver_falls_back_to_public_servers() {
        assert_eq!(
            servers_for(LINK_LOCAL_ONLY),
            Some(vec!["1.1.1.1".to_owned(), "8.8.8.8".to_owned()])
        );
        assert_eq!(
            servers_for(""),
            Some(vec!["1.1.1.1".into(), "8.8.8.8".into()])
        );
    }

    #[test]
    fn global_ipv6_resolvers_are_kept_over_the_fallback() {
        let conf = "nameserver fe80::1%en0\nnameserver 2001:db8::53\nnameserver ::1\nnameserver fd00::53 # ula\n";
        assert_eq!(
            servers_for(conf),
            Some(vec!["2001:db8::53".to_owned(), "fd00::53".to_owned()])
        );
    }

    #[test]
    fn comments_and_other_lines_are_ignored() {
        let conf =
            "domain example.com\n# nameserver 10.0.0.1\nsearch a b\nnameserver\tfe80::2%en1\n";
        assert_eq!(nameservers(conf), vec!["fe80::2%en1".to_owned()]);
        assert!(!slirp_forwards(conf));
    }

    #[test]
    fn apply_chooses_servers_only_for_qemu_guests_without_their_own() {
        let mut guest = json!({"dns": []});
        assert!(apply(&mut guest, true, Some(LINK_LOCAL_ONLY), false));
        assert_eq!(guest["dns"], json!(["1.1.1.1", "8.8.8.8"]));

        let mut own = json!({"dns": ["9.9.9.9"]});
        assert!(!apply(&mut own, true, Some(LINK_LOCAL_ONLY), false));
        assert_eq!(own, json!({"dns": ["9.9.9.9"]}));

        let mut apple = json!({"dns": []});
        assert!(!apply(&mut apple, false, Some(LINK_LOCAL_ONLY), false));
        assert_eq!(apple, json!({"dns": []}));

        let mut unreadable = json!({"dns": []});
        assert!(!apply(&mut unreadable, true, None, false));
        assert_eq!(unreadable, json!({"dns": []}));
    }

    #[test]
    fn apply_in_decides_again_on_every_boot() {
        let dir = tempfile::tempdir().unwrap();
        let mut guest = json!({"dns": []});
        assert!(apply_in(dir.path(), &mut guest, true, Some(LINK_LOCAL_ONLY)).unwrap());
        assert!(dir.path().join(AUTO_FILE).exists());
        // The next boot is on a network with an IPv4 resolver.
        assert!(!apply_in(
            dir.path(),
            &mut guest,
            true,
            Some("nameserver 192.168.1.1\n")
        )
        .unwrap());
        assert_eq!(guest, json!({"dns": []}));
        assert!(!dir.path().join(AUTO_FILE).exists());
        // Or the container moved to Apple Virtualization.
        apply_in(dir.path(), &mut guest, true, Some(LINK_LOCAL_ONLY)).unwrap();
        assert!(!apply_in(dir.path(), &mut guest, false, Some(LINK_LOCAL_ONLY)).unwrap());
        assert_eq!(guest, json!({"dns": []}));
    }

    #[test]
    fn servers_the_person_sets_replace_chosen_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut guest = json!({"dns": []});
        apply_in(dir.path(), &mut guest, true, Some(LINK_LOCAL_ONLY)).unwrap();
        guest["dns"] = json!(["9.9.9.9"]);
        clear_auto(dir.path()).unwrap();
        assert!(!apply_in(dir.path(), &mut guest, true, Some(LINK_LOCAL_ONLY)).unwrap());
        assert_eq!(guest, json!({"dns": ["9.9.9.9"]}));
        clear_auto(dir.path()).unwrap();
    }

    #[test]
    fn machine_script_replaces_dhcp_servers_until_the_next_boot() {
        let script = machine_script(&["2001:db8::53".into(), "1.1.1.1".into()]);
        assert!(script.starts_with("m=/run/veynode-dns; S='2001:db8::53 1.1.1.1'\n"));
        // networkd: a runtime drop-in, DHCP and router-advertised servers off.
        assert!(script.contains("DNS=2001:db8::53 1.1.1.1\\n[DHCPv4]\\nUseDNS=no"));
        assert!(script.contains("[IPv6AcceptRA]\\nUseDNS=no"));
        assert!(script.contains("d=\"/run/systemd/network/${f##*/}.d\""));
        // NetworkManager takes each family's servers on its own.
        assert!(
            script.contains("ipv4.dns '1.1.1.1' ipv6.ignore-auto-dns yes ipv6.dns '2001:db8::53'")
        );
        assert!(script.contains("echo \"$S\" > $m; echo set"));
    }

    #[test]
    fn machine_script_is_valid_shell() {
        let script = machine_script(&["1.1.1.1".into(), "8.8.8.8".into()]);
        let status = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn a_fork_keeps_its_sources_choice() {
        let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        copy_auto(from.path(), to.path()).unwrap();
        assert!(!to.path().join(AUTO_FILE).exists());
        fs::write(from.path().join(AUTO_FILE), b"").unwrap();
        copy_auto(from.path(), to.path()).unwrap();
        assert!(to.path().join(AUTO_FILE).exists());
    }
}
