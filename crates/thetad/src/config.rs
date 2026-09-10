use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use theta_safety::SafetyPolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The one project this instance serves. Fixed at startup and never taken
    /// from a request.
    pub project_id: String,
    pub environment: Environment,
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// Control Plane endpoint, used for token validation and revocation
    /// heartbeats.
    pub control_plane_url: String,
    /// Revocation must propagate within one heartbeat; target <5s
    /// (`04-threat-model-security.md` §2).
    pub heartbeat_interval_ms: u64,
    pub safety: SafetyPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Dev,
    Preview,
    Prod,
}

impl Environment {
    /// Wire name, matching what a token's scope carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Preview => "preview",
            Environment::Prod => "prod",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dev" => Some(Environment::Dev),
            "preview" => Some(Environment::Preview),
            "prod" => Some(Environment::Prod),
            _ => None,
        }
    }

    /// Production-like environments get the strict safety defaults.
    pub fn default_policy(self) -> SafetyPolicy {
        match self {
            Environment::Prod => SafetyPolicy::protected(),
            Environment::Dev | Environment::Preview => SafetyPolicy::development(),
        }
    }
}

impl Config {
    pub fn dev_default(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            environment: Environment::Dev,
            listen: "127.0.0.1:7654".parse().expect("valid default address"),
            data_dir: PathBuf::from("./.thetabase"),
            control_plane_url: "http://127.0.0.1:8080".into(),
            heartbeat_interval_ms: 3_000,
            safety: Environment::Dev.default_policy(),
        }
    }
}
