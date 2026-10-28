//! A Linux machine's commands and copies over SSH (`machine.exec`,
//! `files.copy`), for machines made from a cloud image: the seed
//! (`cloud_init`) installs the login `veynode`, with passwordless sudo and
//! the home key, so input and output stream with no size or time bound and
//! the exit status is the guest command's. Windows and macOS machines have
//! their own SSH paths (`crate::windows_guest`, `services::macos`).
//!
//! Apple Virtualization guests are reached at their vmnet DHCP lease, QEMU
//! guests through a private loopback forward to guest port 22.
use super::{machines::Machine, util};
use crate::api_server::Ctx;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use veynodevm_api::{refuse, ErrorCode};

const READY_TIMEOUT: Duration = Duration::from_secs(600);

/// Whether `machine` is reached over this module's SSH path: a Linux
/// machine whose seed installed the login.
pub fn has_login(machine: &Machine) -> bool {
    machine.os == "linux"
        && super::machines::dir(&machine.name)
            .map(|dir| dir.join("ssh/user").is_file())
            .unwrap_or(false)
}

/// Where and as whom to log in to a running machine.
#[derive(Clone)]
pub struct Target {
    pub user: String,
    pub host: String,
    pub port: u16,
    key: PathBuf,
    known_hosts: PathBuf,
}

impl Target {
    pub fn base(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .arg("-i")
            .arg(&self.key)
            .arg(if program.ends_with("scp") { "-P" } else { "-p" })
            .arg(self.port.to_string())
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .args([
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "ServerAliveInterval=15",
            ]);
        command
    }

    pub fn ssh(&self) -> Command {
        let mut command = self.base("/usr/bin/ssh");
        command
            .arg("-T")
            .arg(format!("{}@{}", self.user, self.host));
        command
    }

    fn remote(&self, path: &str) -> String {
        format!("{}@{}:{path}", self.user, self.host)
    }

    /// Whether the key logs in; `Ok(false)` while the guest still refuses it.
    fn works(&self) -> Result<bool> {
        let output = self.ssh().arg("true").stdin(Stdio::null()).output()?;
        Ok(output.status.success())
    }
}

/// The daemon's status of a running machine; one still starting is waited for.
fn running_vm(ctx: &Ctx, name: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let vm = ctx.vm("status", json!({"id":name}))?;
        match vm["state"].as_str() {
            Some("running") => return Ok(vm),
            Some("starting" | "created") if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(250))
            }
            other => {
                return Err(refuse(
                    ErrorCode::State,
                    format!(
                        "{name} is not running ({})",
                        vm["runtime"]["message"]
                            .as_str()
                            .or(other)
                            .unwrap_or("unknown")
                    ),
                ))
            }
        }
    }
}

/// The machine's guest address: its DHCP lease (Apple Virtualization), by
/// MAC address, else by the host name its seed gave it (a DHCP client that
/// identifies itself by DUID leaves no MAC in the lease).
pub fn lease(name: &str, mac: Option<&str>) -> Option<String> {
    let leases = fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    mac.and_then(|mac| util::lease_ip(&leases, mac))
        .or_else(|| lease_by_name(&leases, &super::cloud_init::hostname(name)))
}

/// The newest lease whose client gave `name` and identified itself by DUID.
/// A lease with an Ethernet address is found by its MAC; one that matches
/// only the name is another machine's, perhaps a removed one of that name.
fn lease_by_name(leases: &str, name: &str) -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    for block in leases.split('}') {
        let (mut ip, mut found, mut lease) = (None, false, 0);
        for line in block.lines() {
            match line.trim().split_once('=') {
                Some(("name", v)) => found = v == name,
                Some(("hw_address", v)) if v.starts_with("1,") => {
                    found = false;
                    break;
                }
                Some(("ip_address", v)) => ip = Some(v.to_owned()),
                Some(("lease", v)) => {
                    lease = u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0)
                }
                _ => {}
            }
        }
        if let (Some(ip), true) = (ip, found) {
            if best.as_ref().is_none_or(|(l, _)| lease >= *l) {
                best = Some((lease, ip));
            }
        }
    }
    best.map(|(_, ip)| ip)
}

/// The guest address of a running Apple Virtualization machine, waited for.
pub fn wait_lease(ctx: &Ctx, name: &str, mac: Option<&str>, deadline: Instant) -> Result<String> {
    loop {
        if let Some(ip) = lease(name, mac) {
            return Ok(ip);
        }
        running_vm(ctx, name)?;
        if Instant::now() > deadline {
            bail!("timed out waiting for {name}'s DHCP lease")
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// The SSH forward of a QEMU machine: its loopback host port to guest port 22.
pub fn ssh_forward(config: &Value) -> Option<u16> {
    config["ports"].as_array()?.iter().find_map(|p| {
        (p["guest_port"] == 22 && p["protocol"].as_str().unwrap_or("tcp") == "tcp")
            .then(|| p["host_port"].as_u64().map(|p| p as u16))
            .flatten()
    })
}

/// Waits for the machine to accept the login (first boot included) and
/// returns where to reach it. `progress` hears once that it is waiting.
pub fn ready(ctx: &Ctx, machine: &Machine, progress: &dyn Fn(&str)) -> Result<Target> {
    let vm = running_vm(ctx, &machine.name)?;
    let ssh = super::machines::dir(&machine.name)?.join("ssh");
    let user = fs::read_to_string(ssh.join("user"))
        .ok()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
        .with_context(|| format!("{} has no SSH login recorded", machine.name))?;
    let deadline = Instant::now() + READY_TIMEOUT;
    let (host, port) = if vm["config"]["backend"] == "qemu-hvf" {
        let port = ssh_forward(&vm["config"]).with_context(|| {
            format!(
                "{} has no host port forwarded to guest port 22",
                machine.name
            )
        })?;
        ("127.0.0.1".to_owned(), port)
    } else {
        let mac = vm["config"]["mac_address"].as_str();
        (wait_lease(ctx, &machine.name, mac, deadline)?, 22)
    };
    let target = Target {
        user,
        host,
        port,
        key: super::macos::key()?,
        known_hosts: ssh.join("known_hosts"),
    };
    let address: SocketAddr = format!("{}:{}", target.host, target.port).parse()?;
    let mut said = false;
    loop {
        // cloud-init installs the key a little after sshd starts.
        if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok() && target.works()? {
            return Ok(target);
        }
        if !said {
            progress(&format!(
                "waiting for {} to accept SSH at {address}",
                machine.name
            ));
            said = true;
        }
        running_vm(ctx, &machine.name)?;
        if Instant::now() > deadline {
            bail!(
                "{} did not accept the SSH login at {address} in time",
                machine.name
            )
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// A POSIX shell single-quoted word.
fn quote(value: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_-./=:,+@%".contains(c);
    if !value.is_empty() && value.chars().all(plain) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The guest shell's command line: `argv` in `cwd` with `env`, as `user`
/// when it is not the login (`root`: through `sudo -n`). A command run
/// through sudo ends with the SSH session and keeps its pid in `pid_file`
/// (`ends_with_session`).
pub fn remote_command(
    login: &str,
    argv: &[String],
    env: &[String],
    cwd: Option<&str>,
    user: Option<&str>,
    pid_file: Option<&str>,
) -> Result<String> {
    if argv.is_empty() {
        return Err(refuse(ErrorCode::Invalid, "argv is empty"));
    }
    for e in env {
        if !e.contains('=') || e.starts_with('=') {
            return Err(refuse(
                ErrorCode::Invalid,
                format!("env takes KEY=VALUE, got {e}"),
            ));
        }
    }
    let mut script = String::new();
    if let Some(dir) = cwd {
        script += &format!("cd {} && ", quote(dir));
    }
    script += "exec ";
    if !env.is_empty() {
        script += "env ";
        for e in env {
            script += &quote(e);
            script.push(' ');
        }
    }
    script += &argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    Ok(match user {
        None => script,
        Some(u) if u == login => script,
        Some(u) => {
            if u.is_empty()
                || !u
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err(refuse(ErrorCode::Invalid, format!("invalid user {u}")));
            }
            // -n: never prompt (stdin is the command's); -H: the user's HOME.
            // sudo starts a session of its own, so the command there watches
            // the login's sshd ($PPID, passed as $1) itself. As in
            // `ends_with_session`, the shell's job notices stay off stderr.
            format!(
                "exec 4>&2 2>/dev/null; sudo -n -H -u {u} /bin/sh -c {} sh \"$PPID\" 2>&4 4>&-; exit $?",
                quote(&ends_with_session(&script, pid_file, "$1"))
            )
        }
    })
}

/// Sends a signal (`TERM`, `INT`, `HUP` or `KILL`) to a running exec.
pub type Signal = Arc<dyn Fn(&str) + Send + Sync>;

/// The `ssh` command that runs `argv` in the machine and how to signal it.
/// The caller connects its stdio.
pub fn exec_command(
    target: &Target,
    argv: &[String],
    env: &[String],
    cwd: Option<&str>,
    user: Option<&str>,
) -> Result<(Command, Signal)> {
    let pid_file = format!("/tmp/.veynode-exec-{}", util::random_hex(8));
    // The command runs as `user` when that is not the login, through sudo:
    // its pid is recorded inside sudo, and it is signalled as that user.
    let sudo = matches!(user, Some(u) if u != target.user);
    let line = remote_command(&target.user, argv, env, cwd, user, Some(&pid_file))?;
    let mut command = target.ssh();
    command.arg(if sudo {
        line
    } else {
        ends_with_session(&line, Some(&pid_file), "$PPID")
    });
    let sudo = if sudo { "sudo -n " } else { "" };
    let target = target.clone();
    let signal: Signal = Arc::new(move |name: &str| {
        let name = match name {
            "INT" | "HUP" | "KILL" => name,
            _ => "TERM",
        };
        let _ = target
            .ssh()
            .arg(format!(
                "test -s {pid_file} && {sudo}kill -{name} \"$(cat {pid_file})\""
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    Ok((command, signal))
}

/// `script` (which ends in `exec`), so that it ends once `parent` (the SSH
/// session's sshd) is gone. With no terminal, sshd signals nothing when the
/// client goes away, and the command would run on under PID 1. So a
/// watchdog polls `parent` and then hangs up its own process group: sshd
/// (and sudo) start the shell in a session of its own, which holds the
/// command and its children. The command runs in the foreground, with its
/// stdin and signals as they come (a background job would ignore INT), in a
/// shell of its own, so that its pid is known and this one exits with its
/// status, 128 + n for signal n, where sshd would report the signal itself
/// and ssh exit 255.
///
/// With `pid_file`, the command's pid is kept there while it runs, for
/// signals (`exec_command`). The shell's own stderr is closed, so that its
/// job notices ("Killed") do not reach the command's.
fn ends_with_session(script: &str, pid_file: Option<&str>, parent: &str) -> String {
    let (record, forget) = match pid_file {
        Some(file) => (format!("echo $$ > {file}; "), format!("rm -f {file}; ")),
        None => (String::new(), String::new()),
    };
    format!(
        "p={parent}; c=$$; \
         ( while kill -0 $c 2>/dev/null; do \
         kill -0 $p 2>/dev/null || ps -p $p >/dev/null 2>&1 || {{ {forget}kill -HUP 0; }}; sleep 1; \
         done; {forget}) </dev/null >/dev/null 2>&1 & \
         exec 4>&2 2>/dev/null; /bin/sh -c {} 2>&4 4>&-; exit $?",
        quote(&format!("{record}{script}"))
    )
}

/// The `scp` command that copies `host` into the guest's `guest` (`inward`)
/// or back. Directories copy whole.
pub fn copy_command(target: &Target, inward: bool, host: &Path, guest: &str) -> Command {
    let (guest, host) = (target.remote(guest), host.display().to_string());
    let (from, to) = if inward { (host, guest) } else { (guest, host) };
    let mut command = target.base("/usr/bin/scp");
    command.args(["-r", "-q"]).arg(from).arg(to);
    command
}

/// A free loopback TCP port (a QEMU machine's SSH forward).
pub fn unused_port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn words(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn commands_are_quoted_and_run_as_other_users_with_sudo() {
        let line = remote_command(
            "veynode",
            &words(&["echo", "a b", "$HOME"]),
            &words(&["K=v w"]),
            Some("/tmp/x y"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(line, "cd '/tmp/x y' && exec env 'K=v w' echo 'a b' '$HOME'");
        let root = remote_command(
            "veynode",
            &words(&["id", "-u"]),
            &[],
            None,
            Some("root"),
            None,
        )
        .unwrap();
        assert_eq!(
            root,
            format!(
                "exec 4>&2 2>/dev/null; sudo -n -H -u root /bin/sh -c {} sh \"$PPID\" 2>&4 4>&-; exit $?",
                quote(&ends_with_session("exec id -u", None, "$1"))
            )
        );
        assert_eq!(
            remote_command(
                "veynode",
                &words(&["true"]),
                &[],
                None,
                Some("veynode"),
                None
            )
            .unwrap(),
            "exec true"
        );
        assert!(remote_command("veynode", &[], &[], None, None, None).is_err());
        assert!(remote_command(
            "veynode",
            &words(&["true"]),
            &words(&["NOEQ"]),
            None,
            None,
            None
        )
        .is_err());
        assert!(
            remote_command("veynode", &words(&["true"]), &[], None, Some("a b"), None).is_err()
        );
    }

    #[test]
    fn qemu_ssh_rides_its_forward_and_leases_are_found_by_name() {
        let config = json!({"ports":[{"host_port":8080,"guest_port":80,"protocol":"tcp"},{"host_port":50123,"guest_port":22,"protocol":"tcp"}]});
        assert_eq!(ssh_forward(&config), Some(50123));
        assert_eq!(ssh_forward(&json!({})), None);
        let leases = "{\n\tname=ub\n\tip_address=192.168.64.3\n\thw_address=ff,f1:f5\n\tlease=0x10\n}\n{\n\tname=ub\n\tip_address=192.168.64.9\n\tlease=0x20\n}\n{\n\tname=ubx\n\tip_address=192.168.64.4\n\tlease=0x30\n}\n{\n\tname=ub\n\tip_address=192.168.64.130\n\thw_address=1,ea:ef:43:3a:c9:e\n\tlease=0x40\n}\n";
        assert_eq!(lease_by_name(leases, "ub").as_deref(), Some("192.168.64.9"));
        assert_eq!(lease_by_name(leases, "u"), None);
    }

    #[test]
    fn session_bound_commands_keep_their_stdin_status_and_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("pid");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(ends_with_session(
                &format!("exec sh -c 'cat; cat {}; exit 7'", pid_file.display()),
                pid_file.to_str(),
                "$PPID",
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let shell = child.id();
        child.stdin.take().unwrap().write_all(b"in\n").unwrap();
        let output = child.wait_with_output().unwrap();
        let output_text = String::from_utf8(output.stdout).unwrap();
        let (input, pid) = output_text.split_once('\n').unwrap();
        assert_eq!(input, "in");
        let pid: u32 = pid.trim().parse().unwrap();
        assert_ne!(pid, shell, "the command's own shell");
        assert_eq!(output.status.code(), Some(7));
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!pid_file.exists(), "forgotten once it ends");

        // A signal's death is the shell's status, with no job notice.
        let output = Command::new("/bin/bash")
            .arg("-c")
            .arg(ends_with_session(
                "exec sh -c 'echo e >&2; kill -9 $$'",
                None,
                "$PPID",
            ))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(137));
        assert_eq!(output.stderr, b"e\n");
    }
}
