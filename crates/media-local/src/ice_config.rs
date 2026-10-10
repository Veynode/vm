//! Trusted private configuration, with short-lived credentials never logged or advertised.
use anyhow::{ensure, Context, Result};
use rtc::peer_connection::configuration::{
    RTCConfiguration, RTCConfigurationBuilder, RTCIceServer, RTCIceTransportPolicy,
};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use vm_session_auth::ice::IceConfig;

/// A session admitted under an ICE credential ends this many seconds before that
/// credential expires. The authorization watch polls every 250 ms; the teardown
/// after it is bounded (encoder stop at most about 3 s, peer close at most 2 s),
/// so 30 s leaves headroom before the TURN credential itself is invalid. It is
/// also the 210 s admission minimum less the existing 180 s media cap, so an
/// admitted session always keeps at least 180 s from its admission.
pub const SESSION_END_MARGIN_SECONDS: u64 = 30;

pub struct IceSource {
    path: PathBuf,
    requests: Mutex<BTreeMap<String, Vec<Instant>>>,
    /// Session deadlines: an admitted offer is keyed by its request id until its
    /// answer is signed, then by its session id. Expired entries are pruned on admission.
    deadlines: Mutex<BTreeMap<String, u64>>,
}
impl IceSource {
    pub fn open(path: PathBuf, now: u64) -> Result<Self> {
        let source = Self {
            path,
            requests: Mutex::new(BTreeMap::new()),
            deadlines: Mutex::new(BTreeMap::new()),
        };
        source.read(now)?;
        Ok(source)
    }
    pub fn read(&self, now: u64) -> Result<IceConfig> {
        read_private(&self.path, now)
    }
    /// At most eight successful request admissions per device/minute and 256
    /// active device entries. External credential minting is deliberately absent.
    pub fn admit(&self, device: &str) -> Result<()> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| anyhow::anyhow!("ICE admission unavailable"))?;
        let now = Instant::now();
        requests.retain(|_, times| {
            times.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
            !times.is_empty()
        });
        ensure!(
            requests.contains_key(device) || requests.len() < 256,
            "ICE admission capacity"
        );
        let times = requests.entry(device.into()).or_default();
        ensure!(times.len() < 8, "ICE request rate exceeded");
        times.push(now);
        Ok(())
    }
    /// Admits an offer under the credential now on disk. `read` refuses expired and
    /// under-minimum credentials, so the refusal happens before any session slot exists.
    /// The admitted offer then carries that credential's session deadline.
    pub fn admit_offer(&self, request_id: &str, now: u64) -> Result<()> {
        let config = self.read(now)?;
        let until = config
            .expires_at
            .checked_sub(SESSION_END_MARGIN_SECONDS)
            .context("ICE credential too short for the session margin")?;
        let mut deadlines = self.lock_deadlines()?;
        deadlines.retain(|_, deadline| now < *deadline);
        ensure!(
            !deadlines.contains_key(request_id),
            "ICE offer already admitted"
        );
        ensure!(deadlines.len() < 256, "ICE admission capacity");
        deadlines.insert(request_id.into(), until);
        Ok(())
    }
    /// Moves an admitted offer's deadline to the session id its answer was signed under.
    pub fn bind_session(&self, request_id: &str, session_id: &str) -> Result<()> {
        let mut deadlines = self.lock_deadlines()?;
        ensure!(
            !deadlines.contains_key(session_id),
            "ICE session already bound"
        );
        let until = deadlines
            .remove(request_id)
            .context("admitted offer unavailable")?;
        deadlines.insert(session_id.into(), until);
        Ok(())
    }
    /// Fails once a session reaches its credential deadline. A session with no
    /// bound deadline fails closed.
    pub fn check_session(&self, session_id: &str, now: u64) -> Result<()> {
        let deadlines = self.lock_deadlines()?;
        let until = deadlines
            .get(session_id)
            .context("session credential unavailable")?;
        ensure!(now < *until, "ICE credential lifetime reached session end");
        Ok(())
    }
    fn lock_deadlines(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, u64>>> {
        self.deadlines
            .lock()
            .map_err(|_| anyhow::anyhow!("ICE admission unavailable"))
    }
}
fn read_private(path: &Path, now: u64) -> Result<IceConfig> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("private ICE configuration unavailable")?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.nlink() == 1
            && m.len() <= 16 * 1024,
        "ICE configuration must be private owner-held regular file <=16KiB"
    );
    let mut bytes = Vec::new();
    file.by_ref().take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024, "ICE configuration size exceeded");
    // serde's raw error text can contain user-controlled credentials; never forward it.
    let config: IceConfig = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid ICE configuration schema"))?;
    config.check_time(now)?;
    Ok(config)
}
pub fn configuration(config: Option<&IceConfig>, now: u64) -> Result<RTCConfiguration> {
    let Some(config) = config else {
        return Ok(RTCConfigurationBuilder::new().build());
    };
    config.check_time(now)?;
    Ok(RTCConfigurationBuilder::new()
        .with_ice_transport_policy(if config.transport_policy == "relay" {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        })
        .with_ice_servers(
            config
                .ice_servers
                .iter()
                .map(|server| RTCIceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
        )
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::fs::PermissionsExt};
    fn bytes() -> Vec<u8> {
        br#"{"version":1,"transport_policy":"all","issued_at":100,"expires_at":700,"ice_servers":[]}"#.to_vec()
    }
    #[test]
    fn private_file_and_replacement_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ice.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(&bytes()).unwrap();
        drop(file);
        read_private(&path, 100).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&path, 100).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_private(&link, 100).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::hard_link(&path, &link).unwrap();
        assert!(read_private(&path, 100).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::write(
            &path,
            br#"{"credential":"secret-never-in-errors","version":false}"#,
        )
        .unwrap();
        let error = read_private(&path, 100).err().unwrap().to_string();
        assert_eq!(error, "invalid ICE configuration schema");
        std::fs::write(&path, vec![b' '; 16385]).unwrap();
        assert!(read_private(&path, 100).is_err());
    }
    #[test]
    fn local_default_and_configured_policy() {
        let default = configuration(None, 100).unwrap();
        assert!(default.ice_servers().is_empty());
        let config: IceConfig = serde_json::from_slice(&bytes()).unwrap();
        configuration(Some(&config), 100).unwrap();
        assert!(configuration(Some(&config), 491).is_err());
    }
    fn private_source(dir: &Path, config: &[u8]) -> IceSource {
        let path = dir.join("ice.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(config).unwrap();
        drop(file);
        IceSource::open(path, 100).unwrap()
    }
    /// Synthetic clock only: issued_at 100, expires_at 700, no real time involved.
    #[test]
    fn admitted_session_ends_at_credential_expiry_minus_margin() {
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(dir.path(), &bytes());
        source.admit_offer("offer-request", 100).unwrap();
        source.bind_session("offer-request", "session").unwrap();
        let end = 700 - SESSION_END_MARGIN_SECONDS;
        for now in [100, 300, end - 1] {
            assert!(
                source.check_session("session", now).is_ok(),
                "live at {now}"
            );
        }
        for now in [end, end + 1, 700] {
            assert!(
                source.check_session("session", now).is_err(),
                "must be ended at {now}, not later than expiry minus margin"
            );
        }
        assert!(
            source.check_session("never-bound", 100).is_err(),
            "a session with no admitted credential fails closed"
        );
    }
    /// Request ids are client-chosen, so they must never overwrite an existing deadline.
    #[test]
    fn admission_cannot_overwrite_an_existing_session_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(dir.path(), &bytes());
        source.admit_offer("offer", 100).unwrap();
        source.bind_session("offer", "session").unwrap();
        assert!(source.admit_offer("session", 100).is_err());
        source.admit_offer("second", 100).unwrap();
        assert!(source.admit_offer("second", 100).is_err());
        source.admit_offer("third", 100).unwrap();
        assert!(source.bind_session("third", "session").is_err());
        assert!(source.check_session("session", 669).is_ok());
        assert!(source.check_session("session", 670).is_err());
    }
    /// Session-auth minimum: 210 s remaining is admitted, 209 s and expiry are refused.
    #[test]
    fn admission_refuses_expired_and_under_minimum_remaining_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let source = private_source(dir.path(), &bytes());
        source.admit_offer("at-210", 490).unwrap();
        assert!(source.admit_offer("at-209", 491).is_err());
        assert!(source.admit_offer("expired", 700).is_err());
        assert!(source.admit_offer("expired-later", 800).is_err());
    }
}

#[cfg(test)]
#[path = "ice_transport_tests.rs"]
mod transport_tests;
