//! Capture-scoped cleanup evidence. A clean Guest record is never input authority.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::json;
#[cfg(test)]
use serde_json::Value;
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ticket {
    version: u32,
    pub service_instance: String,
    pub capture_instance: String,
    cleanup_retention_ms: u32,
}
fn nonce(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Ticket {
    pub fn read(path: &Path) -> Result<Self> {
        let ticket: Self = read_private_json(path).context("invalid cleanup receipt")?;
        ensure!(
            ticket.version == 1
                && ticket.cleanup_retention_ms == 120_000
                && nonce(&ticket.service_instance)
                && nonce(&ticket.capture_instance),
            "invalid cleanup receipt values"
        );
        Ok(ticket)
    }
}
pub fn read_private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure!(path.is_absolute(), "cleanup file must be absolute");
    // Reject symlinks throughout the path, including parent components.
    for component in path.ancestors() {
        ensure!(
            !fs::symlink_metadata(component)?.file_type().is_symlink(),
            "cleanup path contains symlink"
        );
    }
    let parent = fs::symlink_metadata(path.parent().context("cleanup parent missing")?)?;
    ensure!(
        parent.is_dir()
            && parent.uid() == unsafe { libc::geteuid() }
            && parent.mode() & 0o777 == 0o700,
        "cleanup directory must be private"
    );
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o600
            && m.len() > 0
            && m.len() <= 4096,
        "cleanup file must be bounded private regular file"
    );
    let mut bytes = Vec::new();
    (&file).take(4097).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    ensure!(
        m.nlink() == 1
            && after.nlink() == 1
            && named.is_file()
            && named.dev() == m.dev()
            && named.ino() == m.ino()
            && after.len() == m.len()
            && bytes.len() as u64 == m.len()
            && after.mtime() == m.mtime()
            && after.mtime_nsec() == m.mtime_nsec()
            && after.ctime() == m.ctime()
            && after.ctime_nsec() == m.ctime_nsec(),
        "cleanup file changed or has multiple links"
    );
    ensure!(bytes.len() <= 4096, "cleanup file grew beyond limit");
    serde_json::from_slice(&bytes).context("invalid cleanup JSON")
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    event: String,
    id: String,
    protocol_version: u32,
    service_instance: String,
    capture_instance: String,
    status: String,
    cleanup_confirmed: bool,
    retention_ms: u32,
    cleanup_basis: String,
}
fn accept(bytes: &[u8], ticket: &Ticket) -> Result<()> {
    let status: Status = serde_json::from_slice(bytes).context("invalid cleanup status")?;
    ensure!(
        status.event == "cleanup_status"
            && status.id == "media-cleanup-query"
            && status.protocol_version == 1
            && status.service_instance == ticket.service_instance
            && status.capture_instance == ticket.capture_instance
            && status.status == "clean"
            && status.cleanup_confirmed
            && status.retention_ms == 120_000
            && matches!(
                status.cleanup_basis.as_str(),
                "no_stream_created" | "stop_capture_completed" | "delegate_reported_stopped"
            ),
        "Guest capture cleanup unconfirmed"
    );
    Ok(())
}
pub async fn query(path: &Path, ticket: &Ticket) -> Result<()> {
    let mut stream = UnixStream::connect(path).await?;
    let request = json!({"id":"media-cleanup-query","method":"cleanup_status", "service_instance":ticket.service_instance,"capture_instance":ticket.capture_instance});
    stream.write_all(format!("{request}\n").as_bytes()).await?;
    let length = stream.read_u32().await? as usize;
    ensure!((2..=4096).contains(&length), "invalid cleanup record size");
    let mut record = vec![0; length];
    stream.read_exact(&mut record).await?;
    ensure!(record[0] == 1, "invalid cleanup record kind");
    accept(&record[1..], ticket)?;
    let mut trailing = [0; 1];
    ensure!(
        stream.read(&mut trailing).await? == 0,
        "cleanup record missing terminal EOF"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn ticket() -> Ticket {
        Ticket {
            version: 1,
            service_instance: "a".repeat(32),
            capture_instance: "b".repeat(32),
            cleanup_retention_ms: 120_000,
        }
    }
    fn clean() -> Value {
        json!({"event":"cleanup_status","id":"media-cleanup-query","protocol_version":1,"service_instance":"a".repeat(32),"capture_instance":"b".repeat(32),"status":"clean","cleanup_confirmed":true,"retention_ms":120000,"cleanup_basis":"stop_capture_completed"})
    }
    #[test]
    fn evidence_requires_exact_capture_and_positive_proof() {
        assert!(accept(&serde_json::to_vec(&clean()).unwrap(), &ticket()).is_ok());
        for (key, bad) in [
            ("service_instance", json!("c".repeat(32))),
            ("capture_instance", json!("c".repeat(32))),
            ("status", json!("unknown")),
            ("status", json!("active")),
            ("status", json!("failed")),
            ("status", json!("service_mismatch")),
            ("cleanup_confirmed", json!(false)),
            ("cleanup_confirmed", json!(1)),
            ("cleanup_basis", json!("assumed")),
            ("extra", json!(true)),
            ("protocol_version", json!(2)),
            ("retention_ms", json!(0)),
        ] {
            let mut value = clean();
            value[key] = bad;
            assert!(
                accept(&serde_json::to_vec(&value).unwrap(), &ticket()).is_err(),
                "{key}"
            );
        }
    }
    #[tokio::test]
    async fn actual_socket_requires_bounded_single_record_and_eof() {
        for mode in ["clean", "trailing", "oversize", "wrong_kind", "missing_eof"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let task = tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let b = s.read_u8().await.unwrap();
                    if b == b'\n' {
                        break;
                    }
                    request.push(b);
                }
                assert_eq!(
                    serde_json::from_slice::<Value>(&request).unwrap()["capture_instance"],
                    "b".repeat(32)
                );
                if mode == "oversize" {
                    s.write_u32(4097).await.unwrap();
                    return;
                }
                let data = serde_json::to_vec(&clean()).unwrap();
                s.write_u32((data.len() + 1) as u32).await.unwrap();
                s.write_u8(if mode == "wrong_kind" { 2 } else { 1 })
                    .await
                    .unwrap();
                s.write_all(&data).await.unwrap();
                if mode == "trailing" {
                    s.write_u8(0).await.unwrap();
                }
                if mode == "missing_eof" {
                    std::future::pending::<()>().await;
                }
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                query(&path, &ticket()),
            )
            .await;
            assert_eq!(matches!(result, Ok(Ok(()))), mode == "clean", "{mode}");
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
#[path = "guest_cleanup_tests.rs"]
mod file_tests;
