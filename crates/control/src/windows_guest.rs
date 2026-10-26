//! Windows guests (QEMU/HVF `guest: windows`) answer command execution, file
//! transfer and the desktop and accessibility tools through Windows' own
//! OpenSSH Server, which the unattended install enables: nothing else is
//! installed in the Guest. The scripts in `guest/windows/` are sent as text
//! with each call. Commands and files run as the SSH user over the forwarded
//! port; desktop methods run in the logged-on user's desktop session
//! (`session.ps1`), because SSH sessions have no desktop.
//!
//! A machine's SSH login lives in `<home>/machines/<id>/ssh/`: `id_ed25519`,
//! `known_hosts` and `user` (`veynodevm machine ssh-key` writes them).
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const SESSION: &str = include_str!("../../../guest/windows/session.ps1");
const DESKTOP: &str = include_str!("../../../guest/windows/desktop.ps1");
const EXEC: &str = include_str!("../../../guest/windows/exec.ps1");
const FILES: &str = include_str!("../../../guest/windows/files.ps1");

pub const MAX_FILE: u64 = 8 * 1024 * 1024;
const CHUNK: usize = 32 * 1024;
const MAX_TRANSFERS: usize = 16;
const IDLE: Duration = Duration::from_secs(60);
const LIFETIME: Duration = Duration::from_secs(300);

/// The SSH login of one Windows machine.
#[derive(Clone, Debug)]
pub struct Ssh {
    user: String,
    address: std::net::Ipv4Addr,
    port: u16,
    key: PathBuf,
    known_hosts: PathBuf,
}

/// The machine's SSH directory, for a daemon whose state directory is
/// `<home>/state`.
pub fn ssh_dir(state_dir: &Path, id: &str) -> Result<PathBuf> {
    match (state_dir.file_name(), state_dir.parent()) {
        (Some(name), Some(home)) if name == "state" => Ok(home.join("machines").join(id).join("ssh")),
        _ => bail!("SSH is not set up for {id}: this daemon's state directory is not a VeynodeVM home's state directory, so it has no machines"),
    }
}

impl Ssh {
    pub fn locate(state_dir: &Path, config: &crate::Config) -> Result<Self> {
        let id = &config.id;
        let forward = config
            .ports
            .iter()
            .find(|p| p.guest_port == 22 && p.protocol == "tcp")
            .with_context(|| format!("SSH is not set up for {id}: no host port forwards to guest port 22 (create the machine with --publish 2222:22; scripts/install-distro.py windows does this)"))?;
        let dir = ssh_dir(state_dir, id)?;
        let key = dir.join("id_ed25519");
        let user = fs::read_to_string(dir.join("user")).ok();
        let user = user.as_deref().map(str::trim).filter(|u| !u.is_empty());
        let (Some(user), true) = (user, key.is_file()) else {
            bail!("SSH is not set up for {id}: no login in {} (run veynodevm machine ssh-key {id} --user USER --key PRIVATE_KEY with the key the answer file authorized; scripts/install-distro.py windows does this)", dir.display())
        };
        if !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            bail!("invalid SSH user in {}", dir.display())
        }
        let address = if forward.host_address.is_unspecified() {
            std::net::Ipv4Addr::LOCALHOST
        } else {
            forward.host_address
        };
        Ok(Self {
            user: user.into(),
            address,
            port: forward.host_port,
            key,
            known_hosts: dir.join("known_hosts"),
        })
    }

    fn command(&self, remote: &str) -> Command {
        let mut command = Command::new("/usr/bin/ssh");
        command
            .arg("-i")
            .arg(&self.key)
            .arg("-p")
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
            ])
            .arg(format!("{}@{}", self.user, self.address))
            .arg(remote);
        command
    }

    /// Runs a PowerShell script (the SSH user's default shell runs it) with
    /// `stdin`, returning (status, stdout, stderr); 255 is an SSH failure.
    pub fn powershell(
        &self,
        script: &str,
        stdin: &[u8],
        timeout: Duration,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut child = self
            .command(&encoded(script))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run ssh")?;
        let mut input = child.stdin.take().unwrap();
        let bytes = stdin.to_vec();
        let writer = std::thread::spawn(move || input.write_all(&bytes));
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.read_to_end(&mut bytes);
            bytes
        });
        let err = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "the Windows guest did not answer over SSH within {} s",
                    timeout.as_secs()
                )
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let _ = writer.join();
        let (out, err) = (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        );
        let code = status.code().unwrap_or(255);
        if code == 255 {
            bail!(
                "SSH to the Windows guest failed: {} (is the machine running, and its OpenSSH Server up?)",
                String::from_utf8_lossy(&err).trim()
            )
        }
        Ok((code, out, err))
    }
}

/// A command line for the SSH user's PowerShell that runs `script`, given as
/// base64 so no quoting reaches PowerShell's parser.
fn encoded(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "$ProgressPreference='SilentlyContinue'; iex ([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{}')))",
        base64::engine::general_purpose::STANDARD.encode(utf16)
    )
}

/// The Windows command line for arguments, quoted so CommandLineToArgvW and
/// the C runtime give the program exactly these strings.
pub fn command_line(arguments: &[String]) -> String {
    let mut line = String::new();
    for (i, argument) in arguments.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        if !argument.is_empty() && !argument.contains([' ', '\t', '\n', '\x0b', '"']) {
            line.push_str(argument);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for c in argument.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                _ => {
                    line.extend(std::iter::repeat_n('\\', backslashes));
                    line.push(c);
                    backslashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', backslashes * 2));
        line.push('"');
    }
    line
}

fn json_reply(out: &[u8], err: &[u8], code: i32) -> Result<Value> {
    serde_json::from_slice(out).with_context(|| {
        format!(
            "the Windows guest script exited {code} without a result: {}",
            String::from_utf8_lossy(err).trim()
        )
    })
}

fn decoded(value: &Value) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or_default())
        .context("guest returned invalid output encoding")?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `vm_exec`: argv runs directly, without a shell, as the SSH user, with its
/// home directory as the working directory.
pub fn exec(ssh: &Ssh, arguments: &Value) -> Result<Value> {
    let argv: Vec<String> = arguments["argv"]
        .as_array()
        .context("argv must be an array")?
        .iter()
        .map(|a| {
            a.as_str()
                .map(str::to_owned)
                .context("argv items must be strings")
        })
        .collect::<Result<_>>()?;
    let file = argv
        .first()
        .filter(|f| !f.is_empty())
        .context("argv must not be empty")?;
    if argv.iter().any(|a| a.contains('\0')) {
        bail!("argv items must not contain NUL")
    }
    let timeout = arguments["timeout_ms"].as_u64().unwrap_or(5000);
    let request = json!({"file":file,"arguments":command_line(&argv[1..]),"timeout_ms":timeout});
    let (code, out, err) = ssh.powershell(
        EXEC,
        request.to_string().as_bytes(),
        Duration::from_millis(timeout) + Duration::from_secs(20),
    )?;
    let mut result = json_reply(&out, &err, code)?;
    if let Some(error) = result["error"].as_str() {
        bail!("{error}")
    }
    for key in ["stdout", "stderr"] {
        result[key] = json!(decoded(&result[key])?);
    }
    Ok(json!({"id":"windows","result":result}))
}

/// A desktop or accessibility method, run in the desktop session.
pub fn desktop(ssh: &Ssh, method: &str, arguments: &Value) -> Result<Value> {
    let mut stdin = json!({"method":method,"arguments":arguments}).to_string();
    stdin.push('\n');
    stdin.push_str(DESKTOP);
    let (code, out, err) = ssh
        .powershell(SESSION, stdin.as_bytes(), Duration::from_secs(40))
        // Seen once in the first minute after boot, while the logon was settling.
        .map_err(|error| error.context("the Windows desktop session did not answer; right after boot it can take a minute before desktop tools work, so retry"))?;
    let value = json_reply(&out, &err, code)?;
    if let Some(error) = value["error"].as_str() {
        bail!("{error}")
    }
    Ok(value)
}

/// One file name in the Guest's files directory: no separators, devices,
/// wildcards or names Windows would change.
pub fn file_name(path: &str) -> Result<&str> {
    let stem = path
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if path.is_empty()
        || path.chars().count() > 200
        || path.starts_with('.')
        || path.ends_with(['.', ' '])
        || path
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        || device
    {
        bail!("path must be one file name in the Guest's files directory (no folders, devices, wildcards or a leading dot)")
    }
    Ok(path)
}

#[derive(Debug)]
enum Kind {
    Upload { name: String },
    Download,
}

#[derive(Debug)]
struct Transfer {
    owner: String,
    vm: String,
    kind: Kind,
    staged: PathBuf,
    size: u64,
    sha256: String,
    offset: u64,
    created: Instant,
    used: Instant,
}

/// Host-side staging of Windows file transfers: chunks are kept in the
/// daemon's state directory and the whole file crosses SSH once, at commit
/// or at download begin.
#[derive(Debug, Default)]
pub struct Files {
    transfers: BTreeMap<String, Transfer>,
}

fn token() -> Result<String> {
    let mut random = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

fn staging(state_dir: &Path) -> Result<PathBuf> {
    crate::home::private_dir(&state_dir.join("windows-files"))
}

fn ok(result: Value) -> Value {
    json!({"id":"windows","result":result})
}

impl Files {
    fn expire(&mut self) {
        let now = Instant::now();
        let old: Vec<String> = self
            .transfers
            .iter()
            .filter(|(_, t)| now - t.used > IDLE || now - t.created > LIFETIME)
            .map(|(k, _)| k.clone())
            .collect();
        for token in old {
            self.remove(&token);
        }
    }

    fn remove(&mut self, token: &str) -> Option<Transfer> {
        let transfer = self.transfers.remove(token)?;
        let _ = fs::remove_file(&transfer.staged);
        Some(transfer)
    }

    fn get(&mut self, args: &Value, vm: &str) -> Result<(String, &mut Transfer)> {
        self.expire();
        let token = args["token"].as_str().context("missing token")?.to_owned();
        let owner = args["owner"].as_str().unwrap_or_default();
        match self.transfers.get_mut(&token) {
            Some(t) if t.owner == owner && t.vm == vm => {
                t.used = Instant::now();
                Ok((token, t))
            }
            _ => bail!("unknown or expired transfer token"),
        }
    }

    fn admit(&mut self) -> Result<()> {
        self.expire();
        if self.transfers.len() >= MAX_TRANSFERS {
            bail!("busy: {MAX_TRANSFERS} Windows file transfers are open; finish or cancel one")
        }
        Ok(())
    }

    pub fn upload_begin(&mut self, state_dir: &Path, vm: &str, args: &Value) -> Result<Value> {
        let name = file_name(args["path"].as_str().context("missing path")?)?.to_owned();
        let size = args["size"]
            .as_u64()
            .filter(|s| *s <= MAX_FILE)
            .context("size must be 0..8388608")?;
        let sha256 = args["sha256"]
            .as_str()
            .filter(|d| {
                d.len() == 64
                    && d.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
            .context("sha256 must be 64 lowercase hex digits")?
            .to_owned();
        self.admit()?;
        let token = token()?;
        let staged = staging(state_dir)?.join(&token);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        let now = Instant::now();
        self.transfers.insert(
            token.clone(),
            Transfer {
                owner: args["owner"].as_str().unwrap_or_default().into(),
                vm: vm.into(),
                kind: Kind::Upload { name },
                staged,
                size,
                sha256: sha256.clone(),
                offset: 0,
                created: now,
                used: now,
            },
        );
        Ok(ok(
            json!({"token":token,"size":size,"sha256":sha256,"chunk_bytes":CHUNK,"idle_timeout_seconds":IDLE.as_secs()}),
        ))
    }

    pub fn upload_write(&mut self, vm: &str, args: &Value) -> Result<Value> {
        let (_, t) = self.get(args, vm)?;
        if !matches!(t.kind, Kind::Upload { .. }) {
            bail!("method does not match transfer kind")
        }
        if args["offset"].as_u64() != Some(t.offset) {
            bail!("upload offset must be sequential")
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(args["data"].as_str().context("missing data")?)
            .context("data must be base64")?;
        if data.len() > CHUNK || t.offset + data.len() as u64 > t.size {
            bail!("chunk exceeds 32 KiB or the declared size")
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&t.staged)?
            .write_all(&data)?;
        t.offset += data.len() as u64;
        Ok(ok(json!({"offset":t.offset})))
    }

    /// Ends the upload and returns its name, bytes and digest once they are
    /// complete and match; the caller publishes them with [`put`] outside the
    /// daemon's lock.
    pub fn upload_take(&mut self, vm: &str, args: &Value) -> Result<(String, Vec<u8>, String)> {
        let (token, t) = self.get(args, vm)?;
        let Kind::Upload { name } = &t.kind else {
            bail!("method does not match transfer kind")
        };
        let name = name.clone();
        let transfer = self.transfers.remove(&token).unwrap();
        let bytes = fs::read(&transfer.staged);
        let _ = fs::remove_file(&transfer.staged);
        if transfer.offset != transfer.size {
            bail!("upload is incomplete")
        }
        let bytes = bytes?;
        if hex(&Sha256::digest(&bytes)) != transfer.sha256 {
            bail!("staged upload does not match its SHA256; upload again")
        }
        Ok((name, bytes, transfer.sha256))
    }

    /// Stages a downloaded file and returns its transfer.
    pub fn download_insert(
        &mut self,
        state_dir: &Path,
        vm: &str,
        owner: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        self.admit()?;
        let token = token()?;
        let staged = staging(state_dir)?.join(&token);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?
            .write_all(bytes)?;
        let sha256 = hex(&Sha256::digest(bytes));
        let now = Instant::now();
        self.transfers.insert(
            token.clone(),
            Transfer {
                owner: owner.into(),
                vm: vm.into(),
                kind: Kind::Download,
                staged,
                size: bytes.len() as u64,
                sha256: sha256.clone(),
                offset: 0,
                created: now,
                used: now,
            },
        );
        Ok(ok(
            json!({"token":token,"size":bytes.len(),"sha256":sha256,"chunk_bytes":CHUNK,"idle_timeout_seconds":IDLE.as_secs()}),
        ))
    }

    pub fn download_read(&mut self, vm: &str, args: &Value) -> Result<Value> {
        let (token, t) = self.get(args, vm)?;
        if !matches!(t.kind, Kind::Download) {
            bail!("method does not match transfer kind")
        }
        if args["offset"].as_u64() != Some(t.offset) {
            bail!("download offset must be sequential")
        }
        let mut file = fs::File::open(&t.staged)?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(t.offset))?;
        let mut buffer = vec![0; (t.size - t.offset).min(CHUNK as u64) as usize];
        file.read_exact(&mut buffer)?;
        t.offset += buffer.len() as u64;
        let eof = t.offset == t.size;
        let reply = ok(
            json!({"data":base64::engine::general_purpose::STANDARD.encode(&buffer),"offset":t.offset,"eof":eof,"sha256":t.sha256}),
        );
        if eof {
            self.remove(&token);
        }
        Ok(reply)
    }

    pub fn cancel(&mut self, vm: &str, args: &Value) -> Result<Value> {
        let (token, _) = self.get(args, vm)?;
        self.remove(&token);
        Ok(ok(json!({"cancelled":true})))
    }

    /// The MCP session ended: drop its unfinished transfers.
    pub fn session_end(&mut self, vm: &str, owner: &str) -> Value {
        let tokens: Vec<String> = self
            .transfers
            .iter()
            .filter(|(_, t)| t.owner == owner && t.vm == vm)
            .map(|(k, _)| k.clone())
            .collect();
        for token in &tokens {
            self.remove(token);
        }
        ok(json!({"cancelled_transfers":tokens.len()}))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn guest_error(out: &[u8], err: &[u8], code: i32) -> anyhow::Error {
    match serde_json::from_slice::<Value>(out) {
        Ok(v) if v["error"].is_string() => anyhow::anyhow!(
            "{}: {}",
            v["code"].as_str().unwrap_or("failed"),
            v["error"].as_str().unwrap()
        ),
        _ => anyhow::anyhow!(
            "the Windows guest file script exited {code}: {}",
            String::from_utf8_lossy(err).trim()
        ),
    }
}

/// Publishes a checked upload; never replaces an existing file.
pub fn put(ssh: &Ssh, name: &str, bytes: &[u8], sha256: &str) -> Result<Value> {
    let mut stdin = json!({"op":"put","name":name,"size":bytes.len(),"sha256":sha256})
        .to_string()
        .into_bytes();
    stdin.push(b'\n');
    stdin.extend_from_slice(bytes);
    let (code, out, err) = ssh.powershell(FILES, &stdin, Duration::from_secs(120))?;
    if code != 0 {
        return Err(guest_error(&out, &err, code));
    }
    let value = json_reply(&out, &err, code)?;
    if value["published"] != true || value["sha256"] != sha256 {
        bail!("the Windows guest did not confirm the upload")
    }
    Ok(ok(value))
}

/// Reads one file of at most [`MAX_FILE`] bytes.
pub fn get(ssh: &Ssh, name: &str) -> Result<Vec<u8>> {
    let mut stdin = json!({"op":"get","name":name,"max":MAX_FILE})
        .to_string()
        .into_bytes();
    stdin.push(b'\n');
    let (code, out, err) = ssh.powershell(FILES, &stdin, Duration::from_secs(120))?;
    if code != 0 {
        return Err(guest_error(&out, &err, code));
    }
    let newline = out
        .iter()
        .position(|b| *b == b'\n')
        .context("the Windows guest file script returned no header")?;
    let header: Value = serde_json::from_slice(&out[..newline])?;
    let bytes = out[newline + 1..].to_vec();
    if header["size"].as_u64() != Some(bytes.len() as u64) || bytes.len() as u64 > MAX_FILE {
        bail!(
            "the Windows guest sent {} bytes for a {} byte file",
            bytes.len(),
            header["size"]
        )
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_follow_windows_quoting() {
        let line =
            |args: &[&str]| command_line(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(line(&["-Command", "Get-Date"]), "-Command Get-Date");
        assert_eq!(line(&["/c", "echo hello world"]), "/c \"echo hello world\"");
        assert_eq!(line(&[""]), "\"\"");
        assert_eq!(line(&["a\"b"]), "\"a\\\"b\"");
        assert_eq!(
            line(&["C:\\dir with space\\"]),
            "\"C:\\dir with space\\\\\""
        );
        assert_eq!(line(&["back\\\\\"quote"]), "\"back\\\\\\\\\\\"quote\"");
        assert_eq!(line(&["C:\\plain\\path"]), "C:\\plain\\path");
    }

    #[test]
    fn file_names_are_single_windows_names() {
        for good in ["report.txt", "a", "x y.bin", "COM10.txt", "console.log"] {
            assert!(file_name(good).is_ok(), "{good}");
        }
        for bad in [
            "", "..", ".hidden", "a\\b", "a/b", "c:x", "x?", "trail.", "trail ", "CON", "nul.txt",
            "com1", "LPT9.log", "a\u{1}b",
        ] {
            assert!(file_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scripts_travel_as_base64_powershell() {
        let line = encoded("Write-Output 'x'");
        assert!(line.starts_with("$ProgressPreference='SilentlyContinue'; iex"));
        assert!(!line.contains("Write-Output"));
        // UTF-16LE of "W" then "r".
        assert!(line.contains("VwByAGkAdABlAC0A"));
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        (dir, state)
    }

    #[test]
    fn uploads_stage_sequential_chunks_and_check_the_digest() {
        let (_dir, state) = fixture();
        let mut files = Files::default();
        let body = b"hello windows";
        let digest = hex(&Sha256::digest(body));
        let owner = "a".repeat(64);
        let begin = files
            .upload_begin(
                &state,
                "win",
                &json!({"owner":owner,"path":"x.txt","size":body.len(),"sha256":digest}),
            )
            .unwrap();
        let token = begin["result"]["token"].as_str().unwrap().to_owned();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        // Another session or VM cannot use the token.
        assert!(files
            .upload_write(
                "win",
                &json!({"owner":"b".repeat(64),"token":token,"offset":0,"data":b64(body)})
            )
            .is_err());
        assert!(files
            .upload_write(
                "other",
                &json!({"owner":owner,"token":token,"offset":0,"data":b64(body)})
            )
            .is_err());
        assert!(files
            .upload_write(
                "win",
                &json!({"owner":owner,"token":token,"offset":3,"data":b64(body)})
            )
            .is_err());
        files
            .upload_write(
                "win",
                &json!({"owner":owner,"token":token,"offset":0,"data":b64(&body[..5])}),
            )
            .unwrap();
        assert!(
            files
                .upload_take("win", &json!({"owner":owner,"token":token}))
                .is_err(),
            "incomplete"
        );
        assert!(
            files.transfers.is_empty()
                && fs::read_dir(state.join("windows-files"))
                    .unwrap()
                    .next()
                    .is_none()
        );

        let begin = files
            .upload_begin(
                &state,
                "win",
                &json!({"owner":owner,"path":"x.txt","size":body.len(),"sha256":digest}),
            )
            .unwrap();
        let token = begin["result"]["token"].as_str().unwrap().to_owned();
        files
            .upload_write(
                "win",
                &json!({"owner":owner,"token":token,"offset":0,"data":b64(body)}),
            )
            .unwrap();
        assert!(
            files
                .upload_write(
                    "win",
                    &json!({"owner":owner,"token":token,"offset":13,"data":b64(b"!")})
                )
                .is_err(),
            "past size"
        );
        let (name, bytes, sha) = files
            .upload_take("win", &json!({"owner":owner,"token":token}))
            .unwrap();
        assert_eq!(
            (name.as_str(), bytes.as_slice(), sha),
            ("x.txt", &body[..], digest)
        );
        assert!(fs::read_dir(state.join("windows-files"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn downloads_read_in_chunks_and_sessions_clean_up() {
        let (_dir, state) = fixture();
        let mut files = Files::default();
        let owner = "c".repeat(64);
        let body: Vec<u8> = (0..CHUNK + 10).map(|i| i as u8).collect();
        let begin = files.download_insert(&state, "win", &owner, &body).unwrap();
        assert_eq!(begin["result"]["size"], body.len());
        let token = begin["result"]["token"].as_str().unwrap().to_owned();
        let first = files
            .download_read("win", &json!({"owner":owner,"token":token,"offset":0}))
            .unwrap();
        assert_eq!(first["result"]["eof"], false);
        assert!(files
            .download_read("win", &json!({"owner":owner,"token":token,"offset":0}))
            .is_err());
        let last = files
            .download_read("win", &json!({"owner":owner,"token":token,"offset":CHUNK}))
            .unwrap();
        assert_eq!(last["result"]["eof"], true);
        assert!(files.transfers.is_empty());

        files.download_insert(&state, "win", &owner, b"x").unwrap();
        files
            .upload_begin(
                &state,
                "win",
                &json!({"owner":owner,"path":"y","size":1,"sha256":"0".repeat(64)}),
            )
            .unwrap();
        assert_eq!(
            files.session_end("win", &owner)["result"]["cancelled_transfers"],
            2
        );
        assert!(fs::read_dir(state.join("windows-files"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn ssh_login_comes_from_the_machine_directory() {
        let (dir, state) = fixture();
        let mut config: crate::Config =
            serde_json::from_value(json!({"id":"win","guest":"windows","cpus":1,"memory_mib":512}))
                .unwrap();
        let error = Ssh::locate(&state, &config).unwrap_err().to_string();
        assert!(error.contains("--publish 2222:22"), "{error}");
        config.ports = serde_json::from_value(json!([{"host_port":2222,"guest_port":22}])).unwrap();
        let error = Ssh::locate(&state, &config).unwrap_err().to_string();
        assert!(error.contains("veynodevm machine ssh-key win"), "{error}");
        let ssh = dir.path().join("machines/win/ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::write(ssh.join("id_ed25519"), "key").unwrap();
        fs::write(ssh.join("user"), "veynode\n").unwrap();
        let login = Ssh::locate(&state, &config).unwrap();
        assert_eq!((login.user.as_str(), login.port), ("veynode", 2222));
        fs::write(ssh.join("user"), "a b").unwrap();
        assert!(Ssh::locate(&state, &config).is_err());
        assert!(ssh_dir(dir.path(), "win").is_err());
    }
}
