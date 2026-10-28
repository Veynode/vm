//! Reaching inside a running machine: `machine.exec`, `files.copy`,
//! `machine.endpoints` and `machine.grant` (the publisher of an Apple
//! Virtualization machine's ports is `publisher`).
//!
//! Every guest OS is reached over SSH with the system `ssh`/`scp`, so stdin
//! and output stream with no size or time bound and the exit status is the
//! guest command's:
//! - Linux: login `veynode` with the per-home key, which the cloud-init seed
//!   installs (`cloud_init`); passwordless sudo.
//! - macOS: the image login; the first connection installs the per-home key
//!   with its password and runs the machine's first-boot steps
//!   (`macos::ensure_ready`).
//! - Windows: the login and key `machine ssh-key` recorded (scripts/install-distro.py).
//!
//! Apple Virtualization guests are reached at their vmnet DHCP lease for the
//! machine's fixed MAC address, QEMU guests through a loopback forward to
//! guest port 22.
use super::{
    self as machine, call as daemon_call,
    macos::{self, quote},
    progress, refuse, Machine,
};
use crate::home;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const READY_TIMEOUT: Duration = Duration::from_secs(600);

/// The Guest-side path a share is mounted at.
pub fn share_path(os: &str, tag: &str) -> Option<String> {
    match os {
        "linux" => Some(format!("/mnt/{tag}")),
        "macos" => Some(format!("/Volumes/{tag}")),
        _ => None,
    }
}

/// What the macOS helpers take for a macOS machine: its files are the
/// machine directory's, its VM the machine's, its volumes the machine's
/// shares (tags `v0`, `v1`, ...).
pub fn macos_guest(machine: &Machine) -> Result<macos::Guest> {
    Ok(macos_guest_in(machine, machine::dir(&machine.name)?))
}

fn macos_guest_in(machine: &Machine, dir: PathBuf) -> macos::Guest {
    macos::Guest {
        name: machine.name.clone(),
        vm_id: machine.name.clone(),
        mac_address: machine.mac_address.clone(),
        dir,
        volumes: machine
            .shares
            .iter()
            .map(|s| macos::Volume {
                host: s.host_path.clone(),
                target: share_path("macos", &s.tag).unwrap_or_default(),
            })
            .collect(),
    }
}

/// Where and as whom to log in to a running machine.
pub struct Target {
    pub os: String,
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

    pub fn remote(&self, path: &str) -> String {
        format!("{}@{}:{path}", self.user, self.host)
    }

    /// Whether the key logs in; `Ok(false)` while the Guest still refuses it.
    fn works(&self) -> Result<bool> {
        let output = self
            .ssh()
            .arg(if self.os == "windows" {
                "exit 0"
            } else {
                "true"
            })
            .stdin(Stdio::null())
            .output()?;
        Ok(output.status.success())
    }
}

/// The daemon's status of a running machine; one still starting is waited for.
fn running_vm(name: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let vm = daemon_call("status", json!({"id":name}))?;
        match vm["state"].as_str() {
            Some("running") => return Ok(vm),
            Some("starting" | "created") if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(250))
            }
            _ => bail!(
                "{name} is not running ({}; veynodevm machine start {name})",
                vm["runtime"]["message"]
                    .as_str()
                    .or(vm["state"].as_str())
                    .unwrap_or("unknown")
            ),
        }
    }
}

/// The machine's Guest address: its DHCP lease (Apple Virtualization), by
/// MAC address, else by the host name its seed gave it (a DHCP client that
/// identifies itself by DUID leaves no MAC in the lease).
pub fn lease(machine: &Machine) -> Option<String> {
    let leases = fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    machine
        .mac_address
        .as_deref()
        .and_then(|mac| macos::lease_ip(&leases, mac))
        .or_else(|| {
            (machine.os == "linux")
                .then(|| lease_by_name(&leases, &super::cloud_init::hostname(&machine.name)))
                .flatten()
        })
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

pub fn wait_lease(machine: &Machine, deadline: Instant) -> Result<String> {
    loop {
        if let Some(ip) = lease(machine) {
            return Ok(ip);
        }
        running_vm(&machine.name)?;
        if Instant::now() > deadline {
            bail!("timed out waiting for {}'s DHCP lease", machine.name)
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// The SSH forward of a QEMU machine: its loopback host port to guest port 22.
fn ssh_forward(config: &Value) -> Option<u16> {
    config["ports"].as_array()?.iter().find_map(|p| {
        (p["guest_port"] == 22 && p["protocol"].as_str().unwrap_or("tcp") == "tcp")
            .then(|| p["host_port"].as_u64().map(|p| p as u16))
            .flatten()
    })
}

/// Waits for the machine to accept the login (first boot included) and
/// returns where to reach it.
pub fn ready(machine: &Machine, quiet: bool) -> Result<Target> {
    let vm = running_vm(&machine.name)?;
    let dir = machine::dir(&machine.name)?;
    let deadline = Instant::now() + READY_TIMEOUT;
    if machine.os == "macos" {
        let view = macos_guest(machine)?;
        let ip = macos::ensure_ready(&view, quiet)?;
        return Ok(Target {
            os: machine.os.clone(),
            user: macos::login(&view)?.0,
            host: ip,
            port: 22,
            key: macos::key()?,
            known_hosts: dir.join("known_hosts"),
        });
    }
    let ssh = home::private_dir(&dir.join("ssh"))?;
    let user = fs::read_to_string(ssh.join("user"))
        .ok()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
        .with_context(|| {
            format!(
                "{} has no SSH login recorded (machine ssh-key)",
                machine.name
            )
        })?;
    let key = if ssh.join("id_ed25519").is_file() {
        ssh.join("id_ed25519")
    } else {
        macos::key()?
    };
    let (host, port) = if vm["config"]["backend"] == "qemu-hvf" {
        let port = ssh_forward(&vm["config"]).with_context(|| {
            format!(
                "{} has no host port forwarded to guest port 22",
                machine.name
            )
        })?;
        ("127.0.0.1".to_owned(), port)
    } else {
        (wait_lease(machine, deadline)?, 22)
    };
    let target = Target {
        os: machine.os.clone(),
        user,
        host,
        port,
        key,
        known_hosts: ssh.join("known_hosts"),
    };
    let address: SocketAddr = format!("{}:{}", target.host, target.port).parse()?;
    let mut said = false;
    loop {
        // cloud-init installs the key a little after sshd starts.
        if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok() && target.works()? {
            return Ok(target);
        }
        if !quiet && !said {
            progress(&format!(
                "waiting for {} to accept SSH at {address}",
                machine.name
            ));
            said = true;
        }
        running_vm(&machine.name)?;
        if Instant::now() > deadline {
            bail!(
                "{} did not accept the SSH login at {address} in time",
                machine.name
            )
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// The command line for the Guest's shell: `argv` in `workdir` with `env`,
/// as `user` when it is not the login (`root`: the administrator).
pub fn remote_command(
    os: &str,
    login: &str,
    argv: &[String],
    env: &[String],
    workdir: Option<&str>,
    user: Option<&str>,
) -> Result<String> {
    remote_command_with(os, login, argv, env, workdir, user, None)
}

/// `remote_command`; a command run through sudo ends with the SSH session
/// and keeps its pid in `pid_file` (`ends_with_session`).
fn remote_command_with(
    os: &str,
    login: &str,
    argv: &[String],
    env: &[String],
    workdir: Option<&str>,
    user: Option<&str>,
    pid_file: Option<&str>,
) -> Result<String> {
    if argv.is_empty() {
        refuse!("invalid", "machine.exec needs a command (argv)")
    }
    for e in env {
        if !e.contains('=') || e.starts_with('=') {
            refuse!("invalid", "env takes KEY=VALUE, got {e}")
        }
    }
    if os == "windows" {
        if user.is_some_and(|u| !u.eq_ignore_ascii_case(login)) {
            refuse!(
                "invalid",
                "Windows machines run commands as their login {login}; user is not supported there"
            )
        }
        return Ok(windows_command(argv, env, workdir));
    }
    let mut script = String::new();
    if let Some(dir) = workdir {
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
            if !u
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                || u.is_empty()
            {
                refuse!("invalid", "invalid user {u}")
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

/// One argument as the Microsoft C runtime parses a command line.
/// A PowerShell single-quoted string.
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The Windows command line, for OpenSSH's DefaultShell set to PowerShell (as
/// install-distro.py and `vm_control::windows_guest` have it): a nested
/// `powershell -EncodedCommand` starts ARGV as one process with a C-runtime
/// command line, inheriting SSH's stdin and output (`-InputFormat None`: the
/// nested PowerShell reads none of it). The outer shell would flatten a
/// failing exit status to 1, so it exits with `$LASTEXITCODE`.
fn windows_command(argv: &[String], env: &[String], workdir: Option<&str>) -> String {
    let mut script =
        String::from("$ErrorActionPreference='Stop'\n$ProgressPreference='SilentlyContinue'\n");
    script += "$i=New-Object Diagnostics.ProcessStartInfo\n$i.UseShellExecute=$false\n";
    script += &format!("$i.FileName={}\n", ps_quote(&argv[0]));
    let arguments = argv[1..]
        .iter()
        .map(|a| windows_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    script += &format!("$i.Arguments={}\n", ps_quote(&arguments));
    if let Some(dir) = workdir {
        script += &format!("$i.WorkingDirectory={}\n", ps_quote(dir));
    }
    for e in env {
        let (key, value) = e.split_once('=').unwrap_or((e, ""));
        script += &format!(
            "$i.EnvironmentVariables[{}]={}\n",
            ps_quote(key),
            ps_quote(value)
        );
    }
    script += "$p=[Diagnostics.Process]::Start($i)\n$p.WaitForExit()\nexit $p.ExitCode\n";
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    format!(
        "powershell -NoProfile -NonInteractive -InputFormat None -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand {}; exit $LASTEXITCODE",
        super::base64(&utf16)
    )
}

fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.bytes().any(|b| b" \t\"&|<>^%".contains(&b)) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out += &"\\".repeat(slashes * 2 + 1);
                out.push('"');
                slashes = 0;
            }
            _ => {
                out += &"\\".repeat(slashes);
                out.push(c);
                slashes = 0;
            }
        }
    }
    out += &"\\".repeat(slashes * 2);
    out.push('"');
    out
}

/// `machine.exec` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecRequest {
    pub name: String,
    pub argv: Vec<String>,
    /// The login (default) or another account; `root` runs `sudo -n`.
    pub user: Option<String>,
    /// `KEY=VALUE` each.
    #[serde(default)]
    pub env: Vec<String>,
    pub workdir: Option<String>,
    /// Connects stdin; otherwise the command reads end of file.
    #[serde(default)]
    pub stdin: bool,
}

/// Sends a signal (`TERM`, `INT`, `HUP` or `KILL`) to a running exec.
pub type Signal = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// The `ssh` command that runs `request` in its machine, waiting for the
/// Guest as `machine.start` with `wait` does, and how to signal it. The
/// caller connects its stdio.
pub fn exec_command(request: &ExecRequest) -> Result<(Command, Signal)> {
    let machine = machine::load(&machine::resolve(&request.name))?;
    let target = ready(&machine, true)?;
    let pid_file = format!("/tmp/.veynode-exec-{}", machine::random_hex(8));
    // The command runs as `user` when that is not the login, through sudo:
    // its pid is recorded inside sudo, and it is signalled as that user.
    let sudo = matches!(request.user.as_deref(), Some(u) if u != target.user);
    let line = remote_command_with(
        &machine.os,
        &target.user,
        &request.argv,
        &request.env,
        request.workdir.as_deref(),
        request.user.as_deref(),
        Some(&pid_file),
    )?;
    let mut command = target.ssh();
    if machine.os == "windows" {
        command.arg(line);
        // No POSIX signals there: any signal ends the SSH session, and the
        // command with it.
        return Ok((command, std::sync::Arc::new(|_: &str| {})));
    }
    command.arg(if sudo {
        line
    } else {
        ends_with_session(&line, Some(&pid_file), "$PPID")
    });
    let sudo = if sudo { "sudo -n " } else { "" };
    let signal: Signal = std::sync::Arc::new(move |name: &str| {
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

/// `files.copy` arguments: `direction` `in` copies `hostPath` to
/// `guestPath`, `out` the other way. Directories copy whole.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CopyRequest {
    pub name: String,
    pub direction: String,
    pub host_path: PathBuf,
    pub guest_path: String,
}

/// The `scp` command for `request`.
pub fn copy_command(request: &CopyRequest) -> Result<Command> {
    let into_guest = match request.direction.as_str() {
        "in" => true,
        "out" => false,
        other => refuse!("invalid", "direction is in or out, got {other}"),
    };
    if !request.host_path.is_absolute() {
        refuse!("invalid", "hostPath must be absolute")
    }
    if request.guest_path.is_empty() {
        refuse!("invalid", "guestPath is empty")
    }
    let machine = machine::load(&machine::resolve(&request.name))?;
    let ssh = ready(&machine, true)?;
    let guest = ssh.remote(&request.guest_path);
    let host = request.host_path.display().to_string();
    let (from, to) = if into_guest {
        (host, guest)
    } else {
        (guest, host)
    };
    let mut command = ssh.base("/usr/bin/scp");
    command.args(["-r", "-q"]).arg(from).arg(to);
    Ok(command)
}

/// `machine endpoints NAME`: how the host reaches the Guest and back.
pub fn endpoints(machine: &Machine) -> Result<Value> {
    let vm = daemon_call("status", json!({"id":machine.name}))?;
    let qemu = vm["config"]["backend"] == "qemu-hvf";
    let ports: Vec<Value> = machine
        .ports
        .iter()
        .map(|p| json!({"guest":p.guest,"host":p.host,"address":"127.0.0.1"}))
        .collect();
    let (address, host_address) = if qemu {
        // User-mode networking: the Guest is reached only through forwards.
        (Value::Null, json!("10.0.2.2"))
    } else {
        match lease(machine).filter(|_| vm["state"] == "running") {
            Some(ip) => {
                let gateway = ip.rsplit_once('.').map(|(net, _)| format!("{net}.1"));
                (json!(ip), json!(gateway))
            }
            None => (Value::Null, Value::Null),
        }
    };
    Ok(json!({"address":address,"ports":ports,"hostAddress":host_address}))
}

/// AppleScript (run over SSH, which has Accessibility and System Events
/// automation since the first boot) that adds `APP` to the Privacy pane
/// `PANE` with its + button, once the host has typed the password.
const ADD_SCRIPT: &str = r#"on run argv
  set pane to item 1 of argv
  set phase to item 2 of argv
  tell application "System Events"
    tell process "System Settings"
      if phase is "add" then
        set frontmost to true
        repeat with e in (entire contents of window 1)
          try
            if role of e is "AXButton" and (description of e is "add" or name of e is "+" or description of e is "Add") then
              click e
              return "clicked"
            end if
          end try
        end repeat
        return "no add button"
      else
        repeat with e in (entire contents of window 1)
          try
            if role of e is "AXSwitch" or role of e is "AXCheckBox" then
              set n to ""
              try
                set n to name of e
              end try
              if n is "" then
                try
                  set n to description of e
                end try
              end if
              if n contains phase then return "switch " & (value of e as text)
            end if
          end try
        end repeat
        repeat with e in (entire contents of window 1)
          try
            if (role of e is "AXStaticText") and ((value of e as text) is phase) then return "listed"
          end try
        end repeat
        return "missing"
      end if
    end tell
  end tell
end run
"#;

/// Types `text`, then Return, into the Guest through the daemon's input
/// path (Screen Sharing), as a person would: also into secure fields, which
/// ignore scripted keys.
fn type_line(vm: &str, text: &str) -> Result<()> {
    let mut chords = text
        .chars()
        .map(|c| {
            super::macos_grant::chord(c)
                .with_context(|| format!("cannot type {c:?} on a US keyboard"))
        })
        .collect::<Result<Vec<_>>>()?;
    chords.push(vec!["ret"]);
    super::macos_grant::press(vm, &chords)
}

/// `machine.grant` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantRequest {
    pub name: String,
    /// The absolute path of an .app bundle in the Guest.
    pub app: String,
    #[serde(default)]
    pub screen_recording: bool,
    #[serde(default)]
    pub accessibility: bool,
}

/// The Privacy panes `request` asks for, checked.
fn panes(request: &GrantRequest) -> Result<Vec<(&'static str, &'static str)>> {
    let app = &request.app;
    if !app.starts_with('/') || !app.ends_with(".app") || app.contains('\n') {
        refuse!(
            "invalid",
            "app takes the absolute path of an .app bundle in the Guest"
        )
    }
    let mut panes = Vec::new();
    if request.screen_recording {
        panes.push(("screen-recording", "Privacy_ScreenCapture"));
    }
    if request.accessibility {
        panes.push(("accessibility", "Privacy_Accessibility"));
    }
    if panes.is_empty() {
        refuse!(
            "invalid",
            "grant needs screenRecording, accessibility or both"
        )
    }
    Ok(panes)
}

/// `machine.grant`: turns the given Privacy & Security permissions on for a
/// Guest app, through System Settings' own + button and the login's password.
pub fn grant(request: &GrantRequest) -> Result<Value> {
    let machine = machine::load(&machine::resolve(&request.name))?;
    if machine.os != "macos" {
        refuse!(
            "unsupported",
            "grant is for macOS machines; {} runs {}",
            machine.name,
            machine.os
        )
    }
    let panes = panes(request)?;
    let app = request.app.as_str();
    let view = macos_guest(&machine)?;
    let ip = macos::ensure_ready(&view, true)?;
    let (_, password) = macos::login(&view)?;
    let label = app
        .rsplit('/')
        .next()
        .unwrap_or(app)
        .trim_end_matches(".app")
        .to_owned();
    let (code, _, err) = macos::remote(&view, &ip, &format!("test -d {}", quote(app)), None)?;
    if code != 0 {
        bail!(
            "{app} is not in the Guest {}",
            String::from_utf8_lossy(&err).trim()
        )
    }
    let script = |pane: &str, phase: &str| -> Result<String> {
        let (code, out, err) = macos::remote(
            &view,
            &ip,
            &format!("/usr/bin/osascript - {} {}", quote(pane), quote(phase)),
            Some(ADD_SCRIPT.as_bytes()),
        )?;
        if code != 0 {
            bail!(
                "System Settings scripting failed: {}",
                String::from_utf8_lossy(&err).trim()
            )
        }
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    };
    let mut granted = serde_json::Map::new();
    for (key, pane) in panes {
        let _ = macos::remote(&view, &ip, "/usr/bin/killall 'System Settings'", None);
        macos::remote(
            &view,
            &ip,
            &format!(
                "/usr/bin/open 'x-apple.systempreferences:com.apple.preference.security?{pane}'"
            ),
            None,
        )?;
        thread::sleep(Duration::from_secs(4));
        let state = |phase: &str| script(pane, phase);
        if state(&label)? == "switch 1" {
            granted.insert(key.into(), json!(true));
            continue;
        }
        if state("add")? != "clicked" {
            bail!("no + button in the {pane} pane")
        }
        // The password sheet, then the file chooser; the chooser takes a
        // typed path after Shift-Command-G.
        thread::sleep(Duration::from_secs(3));
        type_line(&machine.name, &password)?;
        thread::sleep(Duration::from_secs(3));
        super::macos_grant::press(&machine.name, &[vec!["shift", "meta_l", "g"]])?;
        thread::sleep(Duration::from_secs(1));
        type_line(&machine.name, app)?;
        thread::sleep(Duration::from_secs(2));
        super::macos_grant::press(&machine.name, &[vec!["ret"]])?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let on = loop {
            let now = state(&label)?;
            if now == "switch 1" {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            thread::sleep(Duration::from_secs(1));
        };
        if !on {
            bail!("{label} did not turn on in the {pane} pane")
        }
        granted.insert(key.into(), json!(true));
    }
    let _ = macos::remote(&view, &ip, "/usr/bin/killall 'System Settings'", None);
    Ok(json!({"name":machine.name,"app":app,"granted":granted}))
}

/// A free loopback TCP port (a QEMU machine's SSH forward).
pub fn unused_port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Writes `bytes` to a new private file.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn words(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn posix_commands_are_quoted_and_run_as_other_users_with_sudo() {
        let line = remote_command(
            "linux",
            "veynode",
            &words(&["echo", "a b", "$HOME"]),
            &words(&["K=v w"]),
            Some("/tmp/x y"),
            None,
        )
        .unwrap();
        assert_eq!(line, "cd '/tmp/x y' && exec env 'K=v w' echo 'a b' '$HOME'");
        let root = remote_command(
            "macos",
            "admin",
            &words(&["id", "-u"]),
            &[],
            None,
            Some("root"),
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
                "linux",
                "veynode",
                &words(&["true"]),
                &[],
                None,
                Some("veynode")
            )
            .unwrap(),
            "exec true"
        );
        assert!(remote_command("linux", "veynode", &[], &[], None, None).is_err());
        assert!(remote_command(
            "linux",
            "veynode",
            &words(&["true"]),
            &words(&["NOEQ"]),
            None,
            None
        )
        .is_err());
        assert!(remote_command(
            "linux",
            "veynode",
            &words(&["true"]),
            &[],
            None,
            Some("a b")
        )
        .is_err());
    }

    fn decode(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = 0u32;
        let mut count = 0;
        let mut out = Vec::new();
        for c in text.bytes().filter(|&c| c != b'=') {
            bits = bits << 6 | ALPHABET.iter().position(|&a| a == c).unwrap() as u32;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
            }
        }
        out
    }

    #[test]
    fn windows_commands_follow_the_c_runtime_rules() {
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote("a b"), "\"a b\"");
        assert_eq!(windows_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(windows_quote("C:\\dir with\\"), "\"C:\\dir with\\\\\"");
        assert_eq!(windows_quote(""), "\"\"");
        let line = remote_command(
            "windows",
            "veynode",
            &words(&["cmd", "/c", "echo %A%", "it's"]),
            &words(&["A=1"]),
            Some("C:\\x"),
            None,
        )
        .unwrap();
        let encoded = line
            .strip_prefix("powershell -NoProfile -NonInteractive -InputFormat None -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand ")
            .and_then(|rest| rest.strip_suffix("; exit $LASTEXITCODE"))
            .unwrap();
        let script = String::from_utf16(
            &decode(encoded)
                .chunks(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(script.contains("$i.FileName='cmd'\n"));
        assert!(script.contains("$i.Arguments='/c \"echo %A%\" it''s'\n"));
        assert!(script.contains("$i.WorkingDirectory='C:\\x'\n"));
        assert!(script.contains("$i.EnvironmentVariables['A']='1'\n"));
        assert!(script.ends_with("exit $p.ExitCode\n"));
        assert!(remote_command(
            "windows",
            "veynode",
            &words(&["dir"]),
            &[],
            None,
            Some("Administrator")
        )
        .is_err());
    }

    #[test]
    fn shares_mount_per_os_and_qemu_ssh_rides_its_forward() {
        assert_eq!(share_path("linux", "src").as_deref(), Some("/mnt/src"));
        assert_eq!(share_path("macos", "src").as_deref(), Some("/Volumes/src"));
        assert_eq!(share_path("windows", "src"), None);
        let config = json!({"ports":[{"host_port":8080,"guest_port":80,"protocol":"tcp"},{"host_port":50123,"guest_port":22,"protocol":"tcp"}]});
        assert_eq!(ssh_forward(&config), Some(50123));
        assert_eq!(ssh_forward(&json!({})), None);
        let leases = "{\n\tname=ub\n\tip_address=192.168.64.3\n\thw_address=ff,f1:f5\n\tlease=0x10\n}\n{\n\tname=ub\n\tip_address=192.168.64.9\n\tlease=0x20\n}\n{\n\tname=ubx\n\tip_address=192.168.64.4\n\tlease=0x30\n}\n{\n\tname=ub\n\tip_address=192.168.64.130\n\thw_address=1,ea:ef:43:3a:c9:e\n\tlease=0x40\n}\n";
        assert_eq!(lease_by_name(leases, "ub").as_deref(), Some("192.168.64.9"));
        assert_eq!(lease_by_name(leases, "u"), None);
    }

    fn code(error: anyhow::Error) -> String {
        error.downcast::<super::super::ApiError>().unwrap().code
    }

    #[test]
    fn macos_machines_reach_their_files_shares_and_vm() {
        let machine: Machine = serde_json::from_value(json!({
            "name":"mac","id":"abc","backend":"apple-virtualization","os":"macos","disk_gib":100,
            "created_at":0,"mac_address":"72:06:f4:5e:d8:14",
            "shares":[{"host_path":"/Users/me/src","tag":"src","read_only":false},
                      {"host_path":"/Users/me/data","tag":"data","read_only":true}]
        }))
        .unwrap();
        let guest = macos_guest_in(&machine, PathBuf::from("/h/machines/mac"));
        assert_eq!((guest.name.as_str(), guest.vm_id.as_str()), ("mac", "mac"));
        assert_eq!(guest.mac_address.as_deref(), Some("72:06:f4:5e:d8:14"));
        assert_eq!(guest.dir, PathBuf::from("/h/machines/mac"));
        // mount_virtiofs takes tags v0, v1, ... in share order (machine.create
        // registers them that way) at /Volumes/TAG.
        let targets: Vec<_> = guest.volumes.iter().map(|v| v.target.as_str()).collect();
        assert_eq!(targets, ["/Volumes/src", "/Volumes/data"]);
        assert_eq!(guest.volumes[1].host, PathBuf::from("/Users/me/data"));
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

    #[test]
    fn macos_admin_commands_run_through_sudo_without_a_prompt() {
        // machine.exec with user root on macOS: the first boot's sudoers entry
        // lets `sudo -n` run it with stdin left to the command.
        let line = remote_command(
            "macos",
            "admin",
            &words(&["/usr/sbin/systemsetup", "-settimezone", "Europe/Paris"]),
            &words(&["A=1"]),
            Some("/tmp"),
            Some("root"),
        )
        .unwrap();
        assert_eq!(
            line,
            format!(
                "exec 4>&2 2>/dev/null; sudo -n -H -u root /bin/sh -c {} sh \"$PPID\" 2>&4 4>&-; exit $?",
                quote(&ends_with_session(
                    "cd /tmp && exec env A=1 /usr/sbin/systemsetup -settimezone Europe/Paris",
                    None,
                    "$1"
                ))
            )
        );
        // The login itself needs no sudo.
        assert_eq!(
            remote_command("macos", "admin", &words(&["id"]), &[], None, Some("admin")).unwrap(),
            "exec id"
        );
    }

    #[test]
    fn grants_name_their_panes_and_need_an_app_bundle() {
        let request = |app: &str, screen_recording: bool, accessibility: bool| GrantRequest {
            name: "mac".into(),
            app: app.into(),
            screen_recording,
            accessibility,
        };
        assert_eq!(
            panes(&request("/Applications/Core.app", true, true)).unwrap(),
            [
                ("screen-recording", "Privacy_ScreenCapture"),
                ("accessibility", "Privacy_Accessibility")
            ]
        );
        assert_eq!(
            panes(&request("/Applications/Core.app", false, true)).unwrap(),
            [("accessibility", "Privacy_Accessibility")]
        );
        for (app, sr, ax) in [
            ("/Applications/Core.app", false, false),
            ("Applications/Core.app", true, false),
            ("/Applications/core", true, false),
            ("/Applications/a\n.app", true, false),
        ] {
            assert_eq!(
                code(panes(&request(app, sr, ax)).unwrap_err()),
                "invalid",
                "{app}"
            );
        }
        let parsed: GrantRequest = serde_json::from_value(
            json!({"name":"mac","app":"/Applications/Core.app","screenRecording":true}),
        )
        .unwrap();
        assert!(parsed.screen_recording && !parsed.accessibility);
    }

    #[test]
    fn copies_are_checked_before_the_machine_is_looked_up() {
        let request = |direction: &str, host: &str| CopyRequest {
            name: "m".into(),
            direction: direction.into(),
            host_path: PathBuf::from(host),
            guest_path: "/tmp/x".into(),
        };
        assert_eq!(
            code(copy_command(&request("sideways", "/tmp/a")).unwrap_err()),
            "invalid"
        );
        assert_eq!(
            code(copy_command(&request("in", "relative")).unwrap_err()),
            "invalid"
        );
        let mut empty = request("out", "/tmp/a");
        empty.guest_path.clear();
        assert_eq!(code(copy_command(&empty).unwrap_err()), "invalid");
    }
}
