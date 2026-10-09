//! Host worker/listener provenance. This does not attest Guest desktop identity or authorize input.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::File,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
};
fn nonce() -> Result<String> {
    let mut b = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|v| format!("{v:02x}")).collect())
}
fn valid_nonce(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub version: u64,
    pub vm_id: String,
    pub worker_instance: String,
    pub bridge_instance: String,
    pub worker_pid: u32,
    pub socket_path: String,
    pub socket_device: u64,
    pub socket_inode: u64,
    pub guest_port: u16,
    pub scope: String,
    pub input_authorized: bool,
}
impl Binding {
    pub fn check_socket(&self) -> Result<()> {
        let path = Path::new(&self.socket_path);
        ensure!(
            path.is_absolute() && path.to_str().is_some_and(|p| !p.contains('\0')),
            "invalid bridge socket path"
        );
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600,
            "bridge socket must be owner-held mode0600"
        );
        ensure!(
            metadata.dev() == self.socket_device && metadata.ino() == self.socket_inode,
            "bridge socket incarnation changed"
        );
        let parent = std::fs::symlink_metadata(path.parent().context("bridge parent missing")?)?;
        ensure!(
            parent.is_dir()
                && !parent.file_type().is_symlink()
                && parent.uid() == unsafe { libc::geteuid() }
                && parent.mode() & 0o777 == 0o700,
            "bridge parent must be owner-held mode0700"
        );
        Ok(())
    }
}
pub struct State {
    pub worker_instance: String,
    attempt: Option<(String, String)>,
    binding: Option<Binding>,
}
impl State {
    pub fn new() -> Result<Self> {
        Ok(Self {
            worker_instance: nonce()?,
            attempt: None,
            binding: None,
        })
    }
    pub fn begin(&mut self, path: &str) -> Result<String> {
        ensure!(
            self.attempt.is_none(),
            "bridge start already attempted; stop it before retrying"
        );
        let id = nonce()?;
        self.attempt = Some((id.clone(), path.into()));
        Ok(id)
    }
    pub fn invalidate(&mut self) {
        self.attempt = None;
        self.binding = None;
    }
    pub fn accept(
        &mut self,
        value: &Value,
        vm: &str,
        pid: u32,
        attempt: &str,
        start: bool,
    ) -> Result<Binding> {
        let (current, path) = self
            .attempt
            .as_ref()
            .context("bridge was stopped or worker replaced")?;
        ensure!(current == attempt, "bridge attempt was superseded");
        if start {
            ensure!(
                value["listening"] == true && value["guest_connected"] == false,
                "invalid bridge start acknowledgement"
            );
        }
        let proof:Binding=serde_json::from_value(value.get("binding").cloned().context("unsupported worker: bridge provenance missing; listener may exist, stop before retrying")?).context("invalid worker bridge provenance")?;
        ensure!(
            proof.version == 1
                && proof.vm_id == vm
                && proof.worker_pid == pid
                && proof.worker_instance == self.worker_instance
                && proof.bridge_instance == *current
                && proof.socket_path == *path
                && valid_nonce(&proof.worker_instance)
                && valid_nonce(&proof.bridge_instance)
                && proof.guest_port == 5001
                && proof.scope == "host-worker-listener-only"
                && !proof.input_authorized,
            "worker/VM/bridge provenance mismatch"
        );
        proof.check_socket()?;
        if !start {
            ensure!(
                self.binding.as_ref() == Some(&proof),
                "bridge provenance changed"
            );
        }
        self.binding = Some(proof.clone());
        Ok(proof)
    }
    pub fn expected(&self, worker: &str, bridge: &str) -> Result<&Binding> {
        ensure!(
            valid_nonce(worker) && valid_nonce(bridge),
            "invalid bridge instance"
        );
        let proof = self
            .binding
            .as_ref()
            .context("no verified bridge binding")?;
        if proof.worker_instance != worker || proof.bridge_instance != bridge {
            bail!("stale or foreign bridge binding")
        }
        proof.check_socket()?;
        Ok(proof)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    #[test]
    fn binding_requires_real_ack_vm_worker_nonce_and_current_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("bridge.sock");
        let _socket = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let m = std::fs::symlink_metadata(&path).unwrap();
        let mut state = State::new().unwrap();
        let id = state.begin(path.to_str().unwrap()).unwrap();
        let value = json!({"listening":true,"guest_connected":false,"binding":{"version":1,"vm_id":"mac","worker_instance":state.worker_instance,"bridge_instance":id,"worker_pid":123,"socket_path":path,"socket_device":m.dev(),"socket_inode":m.ino(),"guest_port":5001,"scope":"host-worker-listener-only","input_authorized":false}});
        assert!(state
            .accept(
                &json!({"listening":true,"guest_connected":false}),
                "mac",
                123,
                &id,
                true
            )
            .is_err());
        assert!(state.accept(&value, "other", 123, &id, true).is_err());
        assert!(state.accept(&value, "mac", 124, &id, true).is_err());
        state.accept(&value, "mac", 123, &id, true).unwrap();
        state.expected(&state.worker_instance, &id).unwrap();
        assert!(state.expected(&"f".repeat(64), &id).is_err());
        let mut restarted = State::new().unwrap();
        let next = restarted.begin(path.to_str().unwrap()).unwrap();
        assert!(restarted.accept(&value, "mac", 123, &next, true).is_err());
        std::fs::remove_file(&path).unwrap();
        let _replacement = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(state.expected(&state.worker_instance, &id).is_err());
        state.invalidate();
        assert!(state.accept(&value, "mac", 123, &id, true).is_err());
        assert!(state.expected(&state.worker_instance, &id).is_err());
        let newer = state.begin(path.to_str().unwrap()).unwrap();
        assert_ne!(newer, id);
        assert!(
            state.accept(&value, "mac", 123, &id, true).is_err(),
            "late old start acknowledgement after stop/restart must not republish provenance"
        );
    }
}
