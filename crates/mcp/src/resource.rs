//! The resources an MCP session serves: `veynode://vm/{id}/status`, `/screen`,
//! `/log/serial`, `/log/worker` and `/job/{job_id}`.
use veynodevm_api::validate_id;

pub const PREFIX: &str = "veynode://vm/";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Resource {
    Status(String),
    Screen(String),
    Log(String, &'static str),
    Job(String, String),
}
impl Resource {
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix(PREFIX)?;
        let parts: Vec<&str> = rest.split('/').collect();
        let id = parts.first()?.to_string();
        validate_id(&id).ok()?;
        match parts[1..] {
            ["status"] => Some(Self::Status(id)),
            ["screen"] => Some(Self::Screen(id)),
            ["log", "serial"] => Some(Self::Log(id, "serial")),
            ["log", "worker"] => Some(Self::Log(id, "worker")),
            ["job", job]
                if job.len() == 32 && job.bytes().all(|b| b"0123456789abcdef".contains(&b)) =>
            {
                Some(Self::Job(id, job.into()))
            }
            _ => None,
        }
    }
    pub fn vm(&self) -> &str {
        match self {
            Self::Status(id) | Self::Screen(id) | Self::Log(id, _) | Self::Job(id, _) => id,
        }
    }
    pub fn uri(&self) -> String {
        match self {
            Self::Status(id) => format!("{PREFIX}{id}/status"),
            Self::Screen(id) => format!("{PREFIX}{id}/screen"),
            Self::Log(id, stream) => format!("{PREFIX}{id}/log/{stream}"),
            Self::Job(id, job) => format!("{PREFIX}{id}/job/{job}"),
        }
    }
}
/// The VM a resource URI names, for transports that authorize by VM.
pub fn resource_vm(uri: &str) -> Option<String> {
    Resource::parse(uri).map(|r| r.vm().to_string())
}
