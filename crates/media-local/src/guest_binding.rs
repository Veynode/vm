//! Opt-in macOS observation provenance. This never authorizes input or asserts unlock.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use vm_control::bridge::Binding;
const RPC_DEADLINE: Duration = Duration::from_secs(3);
// All three identity RPCs share one budget; preserve time for the native first frame.
const IDENTITY_DEADLINE: Duration = Duration::from_secs(3);
const RESPONSE_LIMIT: u64 = 64 * 1024;

#[derive(Debug)]
enum BindingFailure {
    DisplayInactive,
    DisplayAsleep,
    Unavailable,
}
impl std::fmt::Display for BindingFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DisplayInactive => "Guest display is inactive",
            Self::DisplayAsleep => "Guest display is asleep",
            Self::Unavailable => "Guest observation binding is unavailable",
        })
    }
}
impl std::error::Error for BindingFailure {}
/// Fixed public classifications only. Never infer sleep from missing frames,
/// arbitrary error strings, a timeout, or malformed console status.
pub fn source_failure(error: &anyhow::Error) -> super::source_status::SourceFailure {
    let code = match error.downcast_ref::<BindingFailure>() {
        Some(BindingFailure::DisplayInactive) => "guest_display_inactive",
        Some(BindingFailure::DisplayAsleep) => "guest_display_asleep",
        _ => "guest_capture_unavailable",
    };
    super::source_status::SourceFailure { code }
}

#[derive(Clone, Debug)]
pub struct Options {
    state: PathBuf,
    vm: String,
    worker: String,
    bridge: String,
    recovery_bridge: Option<PathBuf>,
}
pub fn parse_options(args: &[String]) -> Result<(Option<Options>, Vec<String>)> {
    let names = [
        "--guest-binding-state",
        "--guest-binding-vm",
        "--guest-worker-instance",
        "--guest-bridge-instance",
    ];
    let mut values: [Option<String>; 4] = Default::default();
    let mut rest = Vec::new();
    let mut recovery_bridge = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--guest-cleanup-bridge" {
            ensure!(recovery_bridge.is_none(), "duplicate cleanup bridge option");
            i += 1;
            let path = args
                .get(i)
                .filter(|v| !v.starts_with("--"))
                .context("cleanup bridge requires path")?;
            ensure!(
                Path::new(path).is_absolute(),
                "cleanup bridge path must be absolute"
            );
            recovery_bridge = Some(PathBuf::from(path));
        } else if let Some(index) = names.iter().position(|name| *name == args[i]) {
            ensure!(values[index].is_none(), "duplicate Guest binding option");
            i += 1;
            let value = args
                .get(i)
                .filter(|v| !v.starts_with("--"))
                .context("Guest binding option requires value")?;
            values[index] = Some(value.clone());
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    if values.iter().all(Option::is_none) {
        ensure!(
            recovery_bridge.is_none(),
            "cleanup recovery requires Guest binding"
        );
        return Ok((None, rest));
    }
    ensure!(
        values.iter().all(Option::is_some),
        "all four Guest binding options are required"
    );
    let [state, vm, worker, bridge] = values.map(Option::unwrap);
    ensure!(
        Path::new(&state).is_absolute(),
        "Guest binding state must be absolute"
    );
    ensure!(
        !vm.is_empty()
            && vm.len() <= 64
            && vm
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid Guest binding VM"
    );
    ensure!(
        hex64(&worker) && hex64(&bridge),
        "invalid Guest binding instances"
    );
    Ok((
        Some(Options {
            state: state.into(),
            vm,
            worker,
            bridge,
            recovery_bridge,
        }),
        rest,
    ))
}
fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsoleIdentity {
    version: u32,
    security_session_id: u32,
    uid: u32,
    // Value intentionally requires the field, unlike Option's missing=>None default.
    console_set: Value,
    display_id: u32,
    physical_width: u32,
    physical_height: u32,
    display_geometry_sha256: String,
}
impl ConsoleIdentity {
    fn parse(value: Value) -> Result<Self> {
        ensure!(
            value.get("console_set").is_some(),
            "console identity missing console_set"
        );
        let identity: Self = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("invalid console identity schema"))?;
        ensure!(
            identity.version == 1
                && (1..=u32::MAX - 2).contains(&identity.security_session_id)
                && (1..u32::MAX).contains(&identity.uid)
                && (identity.console_set.is_null()
                    || identity
                        .console_set
                        .as_u64()
                        .is_some_and(|v| v <= u32::MAX as u64))
                && identity.display_id > 0
                && (1..=16384).contains(&identity.physical_width)
                && (1..=16384).contains(&identity.physical_height)
                && hex64(&identity.display_geometry_sha256),
            "invalid console identity values"
        );
        Ok(identity)
    }
}
#[derive(Clone)]
struct SocketIdentity {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
impl SocketIdentity {
    fn read(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "private socket must be absolute");
        let parent = fs::symlink_metadata(path.parent().context("socket parent missing")?)?;
        ensure!(
            parent.is_dir()
                && !parent.file_type().is_symlink()
                && parent.uid() == unsafe { libc::geteuid() }
                && parent.mode() & 0o777 == 0o700,
            "socket parent must be private owner directory"
        );
        let m = fs::symlink_metadata(path)?;
        ensure!(
            m.file_type().is_socket()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o777 == 0o600,
            "socket must be private owner socket"
        );
        Ok(Self {
            path: path.into(),
            dev: m.dev(),
            ino: m.ino(),
        })
    }
    fn check(&self) -> Result<()> {
        let now = Self::read(&self.path)?;
        ensure!(
            now.dev == self.dev && now.ino == self.ino,
            "socket incarnation changed"
        );
        Ok(())
    }
}
pub struct Source {
    options: Options,
    control: SocketIdentity,
    proof: Binding,
    recovery_gate: tokio::sync::Mutex<()>,
    preflight_gate: tokio::sync::Mutex<()>,
}
impl Source {
    pub async fn open(
        options: Options,
        media_socket: &Path,
        auth_vm: &str,
        control_enabled: bool,
        guest_stream: bool,
        caps: &Value,
    ) -> Result<Self> {
        ensure!(
            guest_stream && !control_enabled && options.vm == auth_vm,
            "Guest binding requires same-VM signed observation without input control"
        );
        ensure!(
            caps["console_identity_binding_version"] == 1,
            "Guest encoder identity binding v1 required"
        );
        ensure!(
            options.recovery_bridge.is_none() || caps["cleanup_receipt_version"] == 1,
            "replacement cleanup bridge requires encoder receipt v1"
        );
        let control = SocketIdentity::read(&options.state.join("control.sock"))?;
        let value = rpc(&control, "guest_stream_bridge_binding", json!({"id":options.vm,"worker_instance":options.worker,"bridge_instance":options.bridge}), RPC_DEADLINE).await?;
        let proof = parse_proof(value, &options, media_socket)?;
        let source = Self {
            options,
            control,
            proof,
            recovery_gate: tokio::sync::Mutex::new(()),
            preflight_gate: tokio::sync::Mutex::new(()),
        };
        source.identity(IDENTITY_DEADLINE).await?;
        Ok(source)
    }
    pub(crate) fn vm_id(&self) -> &str {
        &self.options.vm
    }
    // Caller separately enforces session grants and source validity. Cleanup intentionally
    // remains possible after the media bridge has failed, against this pinned controller.
    pub(crate) async fn file_rpc(
        &self,
        method: &str,
        args: Value,
        deadline: Duration,
    ) -> Result<Value> {
        rpc_raw(&self.control, method, args, deadline).await
    }
    pub fn admit_preflight(&self) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        self.preflight_gate
            .try_lock()
            .context("Guest preflight already in progress")
    }
    pub async fn validate(&self) -> Result<()> {
        self.proof.check_socket()?;
        let value = rpc(&self.control, "guest_stream_bridge_binding", json!({"id":self.options.vm,"worker_instance":self.options.worker,"bridge_instance":self.options.bridge}), RPC_DEADLINE).await?;
        let proof = parse_proof(value, &self.options, Path::new(&self.proof.socket_path))?;
        ensure!(
            proof == self.proof,
            "Guest worker or bridge incarnation changed"
        );
        Ok(())
    }
    /// Recovery can query a replacement listener for this exact VM and worker only.
    /// It never changes the pinned media source or grants capture/input authority.
    pub async fn recover_cleanup(&self, ticket: &super::guest_cleanup::Ticket) -> Result<()> {
        let _guard = self
            .recovery_gate
            .try_lock()
            .context("cleanup verification already in progress")?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let proof = if let Some(path) = &self.options.recovery_bridge {
                super::guest_cleanup::read_private_json::<Binding>(path).context("invalid cleanup bridge")?
            } else { self.proof.clone() };
            ensure!(proof.vm_id == self.options.vm && proof.worker_instance == self.options.worker
                && proof.worker_pid == self.proof.worker_pid && hex64(&proof.bridge_instance),
                "cleanup bridge belongs to a different VM or worker");
            let mut options = self.options.clone();
            options.bridge = proof.bridge_instance.clone();
            let verify = || async {
                let value = rpc(&self.control, "guest_stream_bridge_binding",
                    json!({"id":options.vm,"worker_instance":options.worker,"bridge_instance":options.bridge}), RPC_DEADLINE).await?;
                let current = parse_proof(value, &options, Path::new(&proof.socket_path))?;
                ensure!(current == proof, "cleanup listener changed");
                Ok::<(), anyhow::Error>(())
            };
            verify().await?;
            super::guest_cleanup::query(Path::new(&proof.socket_path), ticket).await?;
            verify().await?;
            Ok(())
        }).await.context("cleanup verification deadline exceeded")?
    }
    async fn identity(&self, deadline: Duration) -> Result<ConsoleIdentity> {
        tokio::time::timeout(deadline, self.read_identity())
            .await
            .context("Guest identity preparation total deadline exceeded")?
    }
    async fn read_identity(&self) -> Result<ConsoleIdentity> {
        self.validate().await?;
        let guest = rpc(
            &self.control,
            "guest_info",
            json!({"id":self.options.vm}),
            RPC_DEADLINE,
        )
        .await?;
        let status = &guest["result"]["console_identity_status"];
        if guest.get("error").is_some()
            || status["version"] != 1
            || status["identity_eligible"] != true
        {
            let failure = if guest.get("error").is_none()
                && status["version"] == 1
                && status["identity_eligible"] == false
            {
                match status["identity_rejection"].as_str() {
                    Some("display_inactive") => BindingFailure::DisplayInactive,
                    Some("display_asleep") => BindingFailure::DisplayAsleep,
                    _ => BindingFailure::Unavailable,
                }
            } else {
                BindingFailure::Unavailable
            };
            return Err(failure.into());
        }
        let identity = ConsoleIdentity::parse(guest["result"]["console_identity"].clone())?;
        // Bind the independent info read to the same worker/listener before publication.
        self.validate().await?;
        Ok(identity)
    }
    pub async fn prepare(&self) -> Result<ExpectedFile> {
        self.prepare_at(IDENTITY_DEADLINE, &std::env::temp_dir())
            .await
    }
    async fn prepare_at(&self, deadline: Duration, directory: &Path) -> Result<ExpectedFile> {
        ExpectedFile::create_in(&self.identity(deadline).await?, directory)
    }
    pub async fn watch(&self) -> Result<()> {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.validate().await?;
        }
    }
}
fn parse_proof(value: Value, options: &Options, socket: &Path) -> Result<Binding> {
    let proof: Binding = serde_json::from_value(value["binding"].clone())
        .map_err(|_| anyhow::anyhow!("invalid Guest bridge proof"))?;
    ensure!(
        proof.version == 1
            && proof.vm_id == options.vm
            && proof.worker_instance == options.worker
            && proof.bridge_instance == options.bridge
            && proof.worker_pid > 0
            && proof.guest_port == 5001
            && proof.scope == "host-worker-listener-only"
            && !proof.input_authorized
            && Path::new(&proof.socket_path) == socket,
        "Guest bridge proof does not match expected observation source"
    );
    proof.check_socket()?;
    Ok(proof)
}
async fn rpc(
    socket: &SocketIdentity,
    method: &str,
    args: Value,
    deadline: Duration,
) -> Result<Value> {
    let value = rpc_raw(socket, method, args, deadline).await?;
    ensure!(
        value.get("error").is_none(),
        "controller rejected Guest binding query"
    );
    value
        .get("result")
        .cloned()
        .context("Guest binding response missing result")
}
async fn rpc_raw(
    socket: &SocketIdentity,
    method: &str,
    args: Value,
    deadline: Duration,
) -> Result<Value> {
    socket.check()?;
    tokio::time::timeout(deadline, async {
        let mut connection = UnixStream::connect(&socket.path)
            .await
            .context("Guest binding controller unavailable")?;
        socket.check()?;
        connection
            .write_all(&serde_json::to_vec(
                &json!({"method":method,"arguments":args}),
            )?)
            .await?;
        connection.write_all(b"\n").await?;
        let mut data = Vec::new();
        let count = BufReader::new(connection)
            .take(RESPONSE_LIMIT + 1)
            .read_until(b'\n', &mut data)
            .await?;
        ensure!(
            count > 0 && count <= RESPONSE_LIMIT as usize && data.last() == Some(&b'\n'),
            "Guest binding response size or framing invalid"
        );
        let value: Value = serde_json::from_slice(&data)
            .map_err(|_| anyhow::anyhow!("invalid Guest binding response"))?;
        socket.check()?;
        Ok(value)
    })
    .await
    .context("Guest binding controller deadline exceeded")?
}
pub struct ExpectedFile {
    directory: PathBuf,
    path: PathBuf,
}
impl ExpectedFile {
    #[cfg(test)]
    fn create(identity: &ConsoleIdentity) -> Result<Self> {
        Self::create_in(identity, &std::env::temp_dir())
    }
    fn create_in(identity: &ConsoleIdentity, root: &Path) -> Result<Self> {
        let directory = root
            .canonicalize()?
            .join(format!("veynode-media-binding-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let file = Self {
            path: directory.join("identity.json"),
            directory,
        };
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&file.path)?;
        output.write_all(&serde_json::to_vec(identity)?)?;
        output.sync_all()?;
        Ok(file)
    }
    pub fn receipt_path(&self) -> PathBuf {
        self.directory.join("cleanup.json")
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for ExpectedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.receipt_path());
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}
pub async fn watch(source: Option<&Source>) -> Result<()> {
    match source {
        Some(source) => source.watch().await,
        None => std::future::pending().await,
    }
}
#[cfg(test)]
#[path = "guest_binding_tests.rs"]
mod tests;
