//! Container checkpoints, rollback and forks for agents that try risky steps
//! or several approaches in parallel.
//!
//! A Linux container's only mutable state is its writable layer `rw.raw` (see
//! `container`); the image root, boot assets and configuration disk are
//! read-only. A checkpoint is an APFS clone of that layer, so creating one
//! costs no data blocks and takes milliseconds; blocks diverge only as the
//! container keeps writing.
//!
//! ```text
//! <container>/checkpoints/NAME/rw.raw      clone of rw.raw (0400)
//! <container>/checkpoints/NAME/meta.json   name, time, consistency
//! ```
//!
//! A checkpoint of a running container is crash-consistent: the guest runs
//! `sync`, then the layer is cloned in one atomic APFS operation, so the copy
//! is what a power cut at that instant would leave (ext4's journal replays it
//! at the next mount). Memory and processes are not captured: rollback and
//! forks cold-boot the micro-VM, and the entrypoint runs again.
//!
//! A fork is a new container whose writable layer is a clone of a checkpoint
//! (or of the source's current layer) with a fresh identity: container id,
//! hostname, session token, MAC address and published host ports. The guest
//! agent regenerates `/etc/machine-id` when it boots a layer that belongs to
//! another container id (`guest/veynode-agent/layer.go`).
use super::container::{self, Container, PortSpec};
use super::{daemon_call, macos, util::random_hex};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Forks one `fork` call may create.
pub const MAX_FORKS: u32 = 16;
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Recorded on a container created by `fork`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ForkOrigin {
    /// Id of the container it was forked from.
    pub source: String,
    pub source_name: String,
    /// The checkpoint it started from; `None` for the source's live layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Shared by all forks of one `fork` call; `fork rm GROUP` removes them.
    pub group: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub created_at: u64,
    /// The container was running when the layer was cloned.
    pub source_running: bool,
    /// The guest ran `sync` first (only meaningful when running).
    pub guest_synced: bool,
    pub logical_bytes: u64,
    /// Size of the saved machine state (RAM, CPU, devices) of a warm
    /// checkpoint; `None` for a disk-only checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_state_bytes: Option<u64>,
}

/// Why forks always cold-boot, even from a warm checkpoint. Measured on
/// macOS 27: restoring a saved state with any other MAC address or generic
/// machine identifier fails ("invalid argument"), and when the Guest changes
/// its MAC itself the NAT attachment drops its traffic (no DHCP lease). Warm
/// forks would all share the source's MAC and IP address.
pub const WARM_FORK_UNAVAILABLE: &str = "not copied: forks cold-boot from the checkpoint's disk, because Apple Virtualization restores saved memory only with the source's MAC address and machine identifier, which every fork would then share";

/// Saved machine state and console of a warm checkpoint (Apple Virtualization).
const MACHINE_STATE: &str = "machine.vzstate";
const CONSOLE: &str = "console.log";

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn linux_container(c: &Container) -> Result<()> {
    if c.os == "darwin" {
        bail!("checkpoints and forks support Linux containers; macOS containers are not supported yet")
    }
    if c.engine {
        bail!("the shared engine VM has no checkpoints; checkpoint a micro-VM container")
    }
    Ok(())
}

fn checkpoints_dir(c: &Container) -> Result<PathBuf> {
    Ok(container::container_dir(&c.id)?.join("checkpoints"))
}

fn checkpoint_dir(c: &Container, name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("checkpoint names are 1-64 characters [A-Za-z0-9_.-], starting alphanumeric")
    }
    Ok(checkpoints_dir(c)?.join(name))
}

/// Flush the guest's dirty pages so the clone holds recent writes. Best
/// effort: an image without `sync` still yields a crash-consistent clone.
fn guest_sync(c: &Container) -> bool {
    container::exec(c, &["sync".into()], 15_000).is_ok_and(|r| r["exit_code"].as_i64() == Some(0))
}

/// Clone `source` to `target` (which must not exist).
fn clone_layer(source: &Path, target: &Path, mode: u32) -> Result<()> {
    macos::clone_file(source, target)?;
    fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Clone the container's writable layer into `target`, syncing a running
/// guest first. Returns (running, synced).
fn capture_layer(c: &Container, target: &Path) -> Result<(bool, bool)> {
    let running = container::active(&container::vm_state(c));
    let synced = running && guest_sync(c);
    clone_layer(
        &container::container_dir(&c.id)?.join("rw.raw"),
        target,
        0o400,
    )?;
    Ok((running, synced))
}

/// Why a running container cannot get a warm (memory) checkpoint, if so.
fn warm_unavailable(c: &Container, vm: &Value) -> Option<&'static str> {
    if c.backend != "apple-virtualization" {
        Some("QEMU/HVF: virgl and Venus GPU state blocks QEMU migration, so checkpoints there are disk-only")
    } else if vm["machine_identifier_file"].is_null() || vm["mac_address"].is_null() {
        Some("created before warm checkpoints: no persistent machine identifier and MAC")
    } else {
        None
    }
}

/// Pause, save machine state and clone the layer in one paused interval.
fn capture_warm(c: &Container, staging: &Path) -> Result<Value> {
    let dir = container::container_dir(&c.id)?;
    let reply = daemon_call(
        "machine_state_save",
        json!({"id":c.vm_id,"path":staging.join(MACHINE_STATE),"disk_clone":staging.join("rw.raw")}),
    )?;
    let reply = if reply["result"].is_object() {
        reply["result"].clone()
    } else {
        reply
    };
    if reply["state_bytes"].as_u64().is_none() {
        bail!("machine state save failed: {reply}")
    }
    // The restored run continues this console (its ip and started markers).
    let _ = fs::copy(dir.join(CONSOLE), staging.join(CONSOLE));
    Ok(reply)
}

pub fn create(c: &Container, name: &str, memory: bool) -> Result<Value> {
    linux_container(c)?;
    let dir = checkpoint_dir(c, name)?;
    if dir.exists() {
        bail!("checkpoint {name} already exists for {}", c.name)
    }
    let start = Instant::now();
    let parent = checkpoints_dir(c)?;
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let staging = parent.join(format!(".pending-{name}"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let mut warm = Value::Null;
    let mut cold_reason = None;
    let result = (|| -> Result<Meta> {
        let layer = staging.join("rw.raw");
        let running = container::active(&container::vm_state(c));
        if running {
            let vm = daemon_call("status", json!({"id":c.vm_id}))?["config"].clone();
            let unavailable = if memory {
                warm_unavailable(c, &vm)
            } else {
                Some("disk-only checkpoint requested")
            };
            match unavailable {
                None => match capture_warm(c, &staging) {
                    Ok(reply) => warm = reply,
                    Err(error) => cold_reason = Some(format!("{error:#}")),
                },
                Some(reason) => cold_reason = Some(reason.to_owned()),
            }
        }
        let (running, synced) = if warm.is_null() {
            let _ = fs::remove_file(staging.join(MACHINE_STATE));
            let _ = fs::remove_file(&layer);
            capture_layer(c, &layer)?
        } else {
            (true, true)
        };
        let meta = Meta {
            name: name.to_owned(),
            created_at: container::now(),
            source_running: running,
            guest_synced: synced,
            logical_bytes: fs::metadata(&layer)?.len(),
            machine_state_bytes: warm["state_bytes"].as_u64(),
        };
        fs::write(staging.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
        fs::rename(&staging, &dir)?;
        Ok(meta)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    let meta = result?;
    let mut report = json!({
        "container": c.name,
        "checkpoint": meta,
        "kind": if meta.machine_state_bytes.is_some() { "warm" } else { "disk" },
        "consistency": consistency(&meta),
        "create_ms": ms(start),
        "storage": "APFS clone of the writable layer: shares all blocks until the container writes",
    });
    if meta.machine_state_bytes.is_some() {
        report["machine_state"] =
            json!({"bytes": meta.machine_state_bytes, "timing": warm["timing"]});
    } else if let Some(reason) = cold_reason {
        report["warm_unavailable"] = json!(reason);
    }
    Ok(report)
}

fn consistency(meta: &Meta) -> &'static str {
    if meta.machine_state_bytes.is_some() {
        return "machine state and disk saved in one paused interval";
    }
    match (meta.source_running, meta.guest_synced) {
        (false, _) => "stopped: clean",
        (true, true) => "crash-consistent after guest sync",
        (true, false) => "crash-consistent (guest sync failed)",
    }
}

pub fn list(c: &Container) -> Result<Vec<Meta>> {
    let dir = checkpoints_dir(c)?;
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries {
        let path = entry?.path().join("meta.json");
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(meta) = serde_json::from_slice::<Meta>(&bytes) {
                out.push(meta);
            }
        }
    }
    out.sort_by_key(|m| m.created_at);
    Ok(out)
}

pub fn remove(c: &Container, name: &str) -> Result<()> {
    let dir = checkpoint_dir(c, name)?;
    if !dir.join("meta.json").is_file() {
        bail!("no checkpoint {name} for {}", c.name)
    }
    fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))
}

/// Hard-stop a container whose disk is about to be discarded.
fn hard_stop(c: &Container) -> Result<()> {
    super::proxy::stop_existing(c)?;
    if !container::active(&container::vm_state(c)) {
        return Ok(());
    }
    daemon_call("force_stop", json!({"id":c.vm_id,"confirm":c.vm_id}))?;
    if !container::wait_stopped(c, Duration::from_secs(15)) {
        bail!("container {} did not stop", c.name)
    }
    Ok(())
}

/// Wait for the entrypoint-started marker of the current run.
fn wait_ready(c: &Container) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let console = container::read_console(c);
        if console.started {
            return Ok(());
        }
        if let Some(error) = console.errors.first() {
            bail!("container {} failed to start: {error}", c.name)
        }
        if Instant::now() > deadline {
            bail!(
                "container {} did not start within {READY_TIMEOUT:?}",
                c.name
            )
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Replace the writable layer with a clone of checkpoint `name`. A running
/// container is hard-stopped (its current state is discarded) and booted again;
/// `start` overrides whether it runs afterwards.
pub fn rollback(c: &Container, name: &str, start: Option<bool>, memory: bool) -> Result<Value> {
    linux_container(c)?;
    let checkpoint = checkpoint_dir(c, name)?;
    let source = checkpoint.join("rw.raw");
    if !source.is_file() {
        bail!("no checkpoint {name} for {}", c.name)
    }
    let machine_state = checkpoint.join(MACHINE_STATE);
    let total = Instant::now();
    let was_running = container::active(&container::vm_state(c));
    hard_stop(c)?;
    let stop_ms = ms(total);
    let swap = Instant::now();
    let dir = container::container_dir(&c.id)?;
    let staged = dir.join("rw.raw.rollback");
    let _ = fs::remove_file(&staged);
    clone_layer(&source, &staged, 0o600)?;
    fs::rename(&staged, dir.join("rw.raw"))?;
    let swap_ms = ms(swap);
    let mut report = json!({
        "container": c.name,
        "checkpoint": name,
        "stop_ms": stop_ms,
        "swap_ms": swap_ms,
    });
    if start.unwrap_or(was_running) {
        let boot = Instant::now();
        let mut path = "cold";
        // A machine state is the Apple Virtualization backend's; after
        // `update --gpus` moved the container to QEMU it boots the disk.
        if memory && machine_state.is_file() && c.backend == "apple-virtualization" {
            match resume_warm(c, &checkpoint) {
                Ok(timing) => {
                    path = "warm";
                    report["restore"] = timing;
                }
                Err(error) => {
                    report["warm_error"] = json!(format!("{error:#}"));
                    hard_stop(c)?;
                    // A failed restore may have run the Guest on the swapped
                    // layer; boot from a fresh clone of the checkpoint.
                    let _ = fs::remove_file(&staged);
                    clone_layer(&source, &staged, 0o600)?;
                    fs::rename(&staged, dir.join("rw.raw"))?;
                }
            }
        }
        if path == "cold" {
            container::start(c)?;
            wait_ready(c)?;
        }
        report["path"] = json!(path);
        report["memory_state"] = json!(if path == "warm" {
            "restored: processes continue from the checkpoint"
        } else {
            "not restored: the micro-VM booted again from the checkpoint's disk"
        });
        report["boot_ms"] = json!(ms(boot));
        report["state"] = json!("running");
    } else {
        report["state"] = json!("stopped");
    }
    report["total_ms"] = json!(ms(total));
    Ok(report)
}

/// Start the container from the checkpoint's saved machine state, then step
/// the Guest clock to now. The swapped layer must already be in place.
fn resume_warm(c: &Container, checkpoint: &Path) -> Result<Value> {
    let dir = container::container_dir(&c.id)?;
    let start = Instant::now();
    let console = checkpoint.join(CONSOLE);
    if console.is_file() {
        fs::copy(&console, dir.join(CONSOLE))?;
    }
    container::start_from(c, Some(&checkpoint.join(MACHINE_STATE)))?;
    let started = ms(start);
    // The worker reports running once the restored VM has resumed; it exits
    // with an error when the state cannot be restored.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match container::vm_state(c).as_str() {
            "running" => break,
            state if !container::active(state) => bail!("restore failed (VM {state})"),
            _ if Instant::now() > deadline => bail!("restore did not reach running"),
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let running = ms(start);
    let clock = daemon_call("clock_sync", json!({"id":c.vm_id}));
    let clock = clock
        .map(|v| {
            if v["result"].is_object() {
                v["result"].clone()
            } else {
                v
            }
        })
        .unwrap_or_else(|e| json!({"error": format!("{e:#}")}));
    Ok(
        json!({"start_ms": started, "running_ms": running, "clock_sync": clock, "ready_ms": ms(start)}),
    )
}

#[derive(Clone, Debug, Default)]
pub struct ForkOptions {
    pub count: u32,
    pub checkpoint: Option<String>,
    pub start: bool,
}

/// Host ports for a fork: same guest ports and protocols, fresh free host
/// ports on the same address.
fn fresh_ports(ports: &[PortSpec]) -> Result<Vec<PortSpec>> {
    let mut out = Vec::new();
    for p in ports {
        let host_port = if p.protocol == "tcp" {
            std::net::TcpListener::bind((p.host_address, 0))?
                .local_addr()?
                .port()
        } else {
            std::net::UdpSocket::bind((p.host_address, 0))?
                .local_addr()?
                .port()
        };
        out.push(PortSpec {
            host_port,
            ..p.clone()
        });
    }
    if out.iter().enumerate().any(|(i, a)| {
        out[..i]
            .iter()
            .any(|b| a.host_port == b.host_port && a.protocol == b.protocol)
    }) {
        // The kernel handed out one port twice between our probes: retry.
        return fresh_ports(ports);
    }
    Ok(out)
}

/// The daemon VM configuration of a fork, derived from its source's.
pub fn fork_vm_config(
    source_vm: &Value,
    source_dir: &Path,
    fork: &Container,
    fork_dir: &Path,
) -> Result<Value> {
    let mut vm = source_vm.clone();
    if !vm.is_object() {
        bail!("daemon returned no configuration for the source container")
    }
    vm["id"] = json!(fork.vm_id);
    vm["disk"] = json!(fork_dir.join("rw.raw"));
    vm["serial_log"] = json!(fork_dir.join("console.log"));
    vm["mac_address"] = json!(fork.mac_address);
    if !vm["machine_identifier_file"].is_null() {
        // A fresh generic machine identity, created by the worker at first start.
        vm["machine_identifier_file"] = json!(fork_dir.join(container::MACHINE_IDENTIFIER_FILE));
    }
    let source_config = source_dir.join("config.img");
    let mut replaced = false;
    for disk in vm["extra_disks"].as_array_mut().into_iter().flatten() {
        if disk["path"].as_str().map(Path::new) == Some(source_config.as_path()) {
            disk["path"] = json!(fork_dir.join("config.img"));
            replaced = true;
        }
    }
    if !replaced {
        bail!("source VM configuration has no container configuration disk")
    }
    if fork.backend == "qemu-hvf" {
        vm["ports"] = json!(fork.ports);
        if let Some(port) = fork.session_port {
            super::session::add_forward(&mut vm, port);
        }
    }
    Ok(vm)
}

/// Remaining assigned-memory budget of the daemon (`MemoryBudget`).
fn remaining_budget_mib() -> Result<u64> {
    daemon_call("capabilities", json!({}))?["resource_admission"]["remaining_budget_mib"]
        .as_u64()
        .context("daemon reported no memory admission budget")
}

/// Create `options.count` containers from one state of `source`.
pub fn fork(source: &Container, options: &ForkOptions) -> Result<Value> {
    linux_container(source)?;
    if !(1..=MAX_FORKS).contains(&options.count) {
        bail!("fork count must be 1..={MAX_FORKS}")
    }
    let total = Instant::now();
    if options.start {
        let need = source.memory_mib * u64::from(options.count);
        let remaining = remaining_budget_mib()?;
        if need > remaining {
            bail!(
                "memory admission: {} forks x {} MiB = {need} MiB exceeds the daemon's remaining budget of {remaining} MiB",
                options.count,
                source.memory_mib
            )
        }
    }
    let source_dir = container::container_dir(&source.id)?;
    let source_vm = daemon_call("status", json!({"id":source.vm_id}))?["config"].clone();
    let guest_config = container::read_config_disk(&source_dir.join("config.img"))?;
    let group = random_hex(3);
    // One capture shared by every fork, so all of them start from the same state.
    let capture = Instant::now();
    let staging = source_dir.join(format!(".fork-{group}.raw"));
    let (layer, captured) = match &options.checkpoint {
        Some(name) => {
            let path = checkpoint_dir(source, name)?.join("rw.raw");
            if !path.is_file() {
                bail!("no checkpoint {name} for {}", source.name)
            }
            (path, None)
        }
        None => {
            let (running, synced) = capture_layer(source, &staging)?;
            (staging.clone(), Some((running, synced)))
        }
    };
    let capture_ms = ms(capture);
    let mut created: Vec<Container> = Vec::new();
    let result = (|| -> Result<Vec<Value>> {
        let taken: Vec<String> = container::list()?.into_iter().map(|c| c.name).collect();
        let mut reports = Vec::new();
        for index in 1..=options.count {
            let started = Instant::now();
            let id = random_hex(6);
            let name = format!("{}-{group}-{index}", source.name);
            if taken.contains(&name) {
                bail!("container name {name} is taken")
            }
            let dir = container::container_dir(&id)?;
            fs::create_dir(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            let session_port = match source.session_port {
                Some(_) => Some(super::session::free_port()?),
                None => None,
            };
            let fork = Container {
                id: id.clone(),
                name: name.clone(),
                vm_id: format!("c-{id}"),
                ports: fresh_ports(&source.ports)?,
                session_port,
                mac_address: Some(macos::random_mac()),
                created_at: container::now(),
                auto_remove: false,
                fork_of: Some(ForkOrigin {
                    source: source.id.clone(),
                    source_name: source.name.clone(),
                    checkpoint: options.checkpoint.clone(),
                    group: group.clone(),
                }),
                ..source.clone()
            };
            // Saved first so a failure below is cleaned up like any container.
            container::save(&fork)?;
            created.push(fork.clone());
            clone_layer(&layer, &dir.join("rw.raw"), 0o600)?;
            let session_token = random_hex(32);
            container::private_file(&dir.join(super::session::TOKEN_FILE))?
                .write_all(session_token.as_bytes())?;
            let mut config = guest_config.clone();
            config["id"] = json!(id);
            config["hostname"] = json!(name.chars().take(63).collect::<String>());
            config["session_token"] = json!(session_token);
            container::write_config_disk(&dir.join("config.img"), &config)?;
            let vm = fork_vm_config(&source_vm, &source_dir, &fork, &dir)?;
            daemon_call("create", vm)?;
            if options.start {
                container::start(&fork)?;
            }
            reports.push(json!({
                "name": name,
                "id": id,
                "vm_id": fork.vm_id,
                "hostname": config["hostname"],
                "mac_address": fork.mac_address,
                "ports": fork.ports,
                "create_ms": ms(started),
            }));
        }
        if options.start {
            let boot = Instant::now();
            for fork in &created {
                wait_ready(fork)?;
            }
            for report in &mut reports {
                report["ready_after_ms"] = json!(ms(boot));
            }
        }
        Ok(reports)
    })();
    let _ = fs::remove_file(&staging);
    match result {
        Ok(forks) => Ok(json!({
            "source": source.name,
            "group": group,
            "checkpoint": options.checkpoint,
            "source_consistency": captured.map(|(running, synced)| consistency(&Meta {
                name: String::new(), created_at: 0, source_running: running,
                guest_synced: synced, logical_bytes: 0,
            machine_state_bytes: None,
            })),
            "capture_ms": capture_ms,
            "total_ms": ms(total),
            "state": if options.start { "running" } else { "created" },
            "path": "cold",
            "memory_state": WARM_FORK_UNAVAILABLE,
            "forks": forks,
            "cleanup": format!("veynodevm fork rm {group}"),
        })),
        Err(error) => {
            for fork in &created {
                if container::remove(fork, true).is_err() {
                    if let Ok(dir) = container::container_dir(&fork.id) {
                        let _ = fs::remove_dir_all(dir);
                    }
                }
            }
            Err(error.context(format!(
                "fork of {} failed; removed the {} fork(s) it had created",
                source.name,
                created.len()
            )))
        }
    }
}

/// Forks selected by a group id, or every fork of a source container.
pub fn select_forks(all: &[Container], selector: &str) -> Vec<Container> {
    all.iter()
        .filter(|c| {
            c.fork_of.as_ref().is_some_and(|f| {
                f.group == selector || f.source == selector || f.source_name == selector
            })
        })
        .cloned()
        .collect()
}

/// Stop and delete forks with their writable layers and checkpoints.
pub fn remove_forks(selector: &str) -> Result<Value> {
    let forks = select_forks(&container::list()?, selector);
    if forks.is_empty() {
        bail!("no forks match {selector} (a fork group id or source container)")
    }
    let mut removed = Vec::new();
    for fork in &forks {
        container::remove(fork, true)?;
        removed.push(fork.name.clone());
    }
    Ok(json!({"removed": removed}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, backend: &str, fork_of: Option<ForkOrigin>) -> Container {
        serde_json::from_value(json!({
            "id": id, "name": format!("n{id}"), "image": "alpine", "config_digest": "sha256:0",
            "argv": ["sh"], "env": [], "workdir": "/", "user": "",
            "ports": [{"host_address":"127.0.0.1","host_port":8080,"guest_port":80,"protocol":"tcp"}],
            "volumes": [], "backend": backend, "gpus": null, "desktop": false, "engine": false,
            "auto_remove": false, "cpus": 2, "memory_mib": 512, "created_at": 0,
            "vm_id": format!("c-{id}"), "session_port": if backend == "qemu-hvf" { Some(41000) } else { None },
            "fork_of": fork_of,
        }))
        .unwrap()
    }

    #[test]
    fn names_are_bounded_path_components() {
        assert!(valid_name("before-migration_2.1"));
        for bad in ["", ".hidden", "-x", "a/b", "..", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn fresh_ports_keep_guest_side_and_avoid_the_source() {
        let source = container("a", "qemu-hvf", None);
        let ports = fresh_ports(&source.ports).unwrap();
        assert_eq!(ports[0].guest_port, 80);
        assert_eq!(ports[0].protocol, "tcp");
        assert_ne!(ports[0].host_port, 8080);
        assert!(container::port_free(&ports[0]).is_ok());
    }

    #[test]
    fn fork_config_replaces_identity_disks_and_ports() {
        let mut fork = container("b", "qemu-hvf", None);
        fork.ports[0].host_port = 50123;
        fork.session_port = Some(50124);
        fork.mac_address = Some("02:00:00:00:00:01".into());
        let vm = json!({
            "id": "c-a", "backend": "qemu-hvf", "disk": "/h/containers/a/rw.raw",
            "serial_log": "/h/containers/a/console.log",
            "extra_disks": [
                {"path": "/h/images/root.sqfs", "read_only": true},
                {"path": "/h/containers/a/config.img", "read_only": true},
            ],
            "ports": [
                {"protocol":"tcp","host_address":"127.0.0.1","host_port":8080,"guest_port":80},
                {"protocol":"tcp","host_address":"127.0.0.1","host_port":41000,"guest_port":super::super::session::GUEST_PORT},
            ],
        });
        let out = fork_vm_config(
            &vm,
            Path::new("/h/containers/a"),
            &fork,
            Path::new("/h/containers/b"),
        )
        .unwrap();
        assert_eq!(out["id"], "c-b");
        assert_eq!(out["disk"], "/h/containers/b/rw.raw");
        assert_eq!(out["serial_log"], "/h/containers/b/console.log");
        assert_eq!(out["mac_address"], "02:00:00:00:00:01");
        assert_eq!(out["extra_disks"][0]["path"], "/h/images/root.sqfs");
        assert_eq!(out["extra_disks"][1]["path"], "/h/containers/b/config.img");
        let host_ports: Vec<_> = out["ports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["host_port"].as_u64().unwrap())
            .collect();
        assert_eq!(host_ports, [50123, 50124]);
        // The source's own configuration disk is never shared with a fork.
        let mut missing = vm.clone();
        missing["extra_disks"].as_array_mut().unwrap().remove(1);
        assert!(fork_vm_config(
            &missing,
            Path::new("/h/containers/a"),
            &fork,
            Path::new("/h/containers/b")
        )
        .is_err());
    }

    #[test]
    fn warm_checkpoints_need_apple_and_a_persistent_identity() {
        let apple = container("a", "apple-virtualization", None);
        let identity = json!({"machine_identifier_file":"/h/containers/a/vz-machine-id.bin","mac_address":"02:00:00:00:00:01"});
        assert_eq!(warm_unavailable(&apple, &identity), None);
        assert!(
            warm_unavailable(&apple, &json!({"mac_address":"02:00:00:00:00:01"}))
                .unwrap()
                .contains("created before")
        );
        let qemu = container("a", "qemu-hvf", None);
        assert!(warm_unavailable(&qemu, &identity).unwrap().contains("QEMU"));
    }

    #[test]
    fn fork_gets_its_own_machine_identifier() {
        let fork = container("b", "apple-virtualization", None);
        let vm = json!({"id":"c-a","machine_identifier_file":"/h/containers/a/vz-machine-id.bin",
            "extra_disks":[{"path":"/h/containers/a/config.img","read_only":true}]});
        let out = fork_vm_config(
            &vm,
            Path::new("/h/containers/a"),
            &fork,
            Path::new("/h/containers/b"),
        )
        .unwrap();
        assert_eq!(
            out["machine_identifier_file"],
            "/h/containers/b/vz-machine-id.bin"
        );
    }

    #[test]
    fn apple_fork_config_leaves_ports_to_the_proxy() {
        let fork = container("b", "apple-virtualization", None);
        let vm = json!({"id":"c-a","extra_disks":[{"path":"/h/containers/a/config.img","read_only":true}]});
        let out = fork_vm_config(
            &vm,
            Path::new("/h/containers/a"),
            &fork,
            Path::new("/h/containers/b"),
        )
        .unwrap();
        assert!(out.get("ports").is_none());
    }

    #[test]
    fn fork_selection_by_group_or_source() {
        let origin = |group: &str| ForkOrigin {
            source: "a".into(),
            source_name: "na".into(),
            checkpoint: None,
            group: group.into(),
        };
        let all = vec![
            container("a", "qemu-hvf", None),
            container("b", "qemu-hvf", Some(origin("g1"))),
            container("c", "qemu-hvf", Some(origin("g2"))),
        ];
        let ids = |s| {
            select_forks(&all, s)
                .into_iter()
                .map(|c| c.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("g1"), ["b"]);
        assert_eq!(ids("na"), ["b", "c"]);
        assert_eq!(ids("a"), ["b", "c"]);
        assert!(ids("nb").is_empty());
    }

    #[test]
    fn consistency_labels() {
        let meta = |running, synced| Meta {
            name: "x".into(),
            created_at: 0,
            source_running: running,
            guest_synced: synced,
            logical_bytes: 0,
            machine_state_bytes: None,
        };
        assert_eq!(consistency(&meta(false, false)), "stopped: clean");
        assert_eq!(
            consistency(&meta(true, true)),
            "crash-consistent after guest sync"
        );
    }
}
