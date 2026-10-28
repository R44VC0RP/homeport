//! Peer authorization: only nodes owned by the same Tailscale user as this one.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::Mutex;

const ALLOW_TTL: Duration = Duration::from_secs(600);
const DENY_TTL: Duration = Duration::from_secs(30);
const WHOIS_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("run {cmd}: {source}")]
    Spawn { cmd: String, source: std::io::Error },
    #[error("{cmd} timed out")]
    Timeout { cmd: String },
    #[error("{cmd} failed: {stderr}")]
    Failed { cmd: String, stderr: String },
    #[error("parse tailscale output: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("tailscale output has no {0}")]
    Missing(&'static str),
}

pub struct Tailscale {
    pub bin: PathBuf,
}

impl Tailscale {
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, AuthError> {
        let cmd = format!("tailscale {}", args.join(" "));
        let fut = Command::new(&self.bin).args(args).kill_on_drop(true).output();
        let out = tokio::time::timeout(WHOIS_TIMEOUT, fut)
            .await
            .map_err(|_| AuthError::Timeout { cmd: cmd.clone() })?
            .map_err(|source| AuthError::Spawn { cmd: cmd.clone(), source })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(AuthError::Failed { cmd, stderr });
        }
        Ok(out.stdout)
    }

    pub async fn ipv4(&self) -> Result<IpAddr, AuthError> {
        let out = self.run(&["ip", "-4"]).await?;
        String::from_utf8_lossy(&out)
            .lines()
            .find_map(|l| l.trim().parse().ok())
            .ok_or(AuthError::Missing("IPv4 address"))
    }

    pub async fn self_user(&self) -> Result<u64, AuthError> {
        let v: serde_json::Value = serde_json::from_slice(&self.run(&["status", "--json"]).await?)?;
        v["Self"]["UserID"].as_u64().ok_or(AuthError::Missing("Self.UserID"))
    }

    async fn whois_user(&self, ip: IpAddr) -> Result<(u64, String), AuthError> {
        let v: serde_json::Value = serde_json::from_slice(&self.run(&["whois", "--json", &ip.to_string()]).await?)?;
        let id = v["UserProfile"]["ID"].as_u64().ok_or(AuthError::Missing("UserProfile.ID"))?;
        let login = v["UserProfile"]["LoginName"].as_str().unwrap_or("?").to_owned();
        Ok((id, login))
    }
}

pub struct Authorizer {
    ts: Tailscale,
    self_user: u64,
    self_ips: Vec<IpAddr>,
    cache: Mutex<HashMap<IpAddr, (bool, Instant)>>,
}

impl Authorizer {
    pub fn new(ts: Tailscale, self_user: u64, self_ips: Vec<IpAddr>) -> Self {
        Self { ts, self_user, self_ips, cache: Mutex::new(HashMap::new()) }
    }

    /// Whether `ip` may use the server. Loopback and this node are always allowed.
    pub async fn allow(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        if ip.is_loopback() || self.self_ips.contains(&ip) {
            return true;
        }
        let mut cache = self.cache.lock().await;
        if let Some(&(ok, at)) = cache.get(&ip) {
            let ttl = if ok { ALLOW_TTL } else { DENY_TTL };
            if at.elapsed() < ttl {
                return ok;
            }
        }
        let ok = match self.ts.whois_user(ip).await {
            Ok((user, login)) => {
                let ok = user == self.self_user;
                tracing::info!(%ip, user, login, allowed = ok, "tailscale whois");
                ok
            }
            Err(e) => {
                tracing::warn!(%ip, error = %e, "tailscale whois failed; denying");
                false
            }
        };
        cache.insert(ip, (ok, Instant::now()));
        ok
    }
}
