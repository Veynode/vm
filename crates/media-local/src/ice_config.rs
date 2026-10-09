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

pub struct IceSource {
    path: PathBuf,
    requests: Mutex<BTreeMap<String, Vec<Instant>>>,
}
impl IceSource {
    pub fn open(path: PathBuf, now: u64) -> Result<Self> {
        let source = Self {
            path,
            requests: Mutex::new(BTreeMap::new()),
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
}

#[cfg(test)]
#[path = "ice_transport_tests.rs"]
mod transport_tests;
