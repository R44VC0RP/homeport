//! Peer authorization: an allowlist of Tailscale logins.
//!
//! Every new peer is looked up with `tailscale whois` (cached). It is allowed
//! when its node is owned by a login in the allowlist; tagged nodes (owned by
//! the shared `tagged-devices` user) are never allowed. The allowlist comes
//! from the config file and `--allow-login`; when both are empty it defaults
//! to the login owning this server node, and a tagged server node must name
//! its allowed logins explicitly.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::platform::{Sys, SysOps};

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
    #[error(
        "this node is tagged ({0}), so it has no owner to allow by default: set allow_logins in the \
         config file or pass --allow-login <login> (e.g. you@example.com)"
    )]
    TaggedNeedsAllowlist(String),
}

pub struct Tailscale {
    pub bin: PathBuf,
}

/// This node as Tailscale sees it.
pub struct SelfNode {
    /// Owner's login; `None` for tagged nodes.
    pub login: Option<String>,
    pub tags: Vec<String>,
}

/// A peer node as `tailscale whois` reports it.
struct Peer {
    login: String,
    tags: Vec<String>,
}

fn tags_of(v: &serde_json::Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

impl Tailscale {
    /// The configured CLI, else the first of the platform's usual places, else `tailscale` on PATH.
    pub fn find(configured: Option<PathBuf>) -> Self {
        let bin = configured
            .or_else(|| Sys::tailscale_candidates().into_iter().find(|p| p.exists()))
            .unwrap_or_else(|| PathBuf::from("tailscale"));
        Self { bin }
    }

    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, AuthError> {
        let cmd = format!("tailscale {}", args.join(" "));
        let fut = Sys::tailscale_command(&self.bin).args(args).kill_on_drop(true).output();
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

    pub async fn self_node(&self) -> Result<SelfNode, AuthError> {
        let v: serde_json::Value = serde_json::from_slice(&self.run(&["status", "--json"]).await?)?;
        let me = &v["Self"];
        let tags = tags_of(&me["Tags"]);
        if !tags.is_empty() {
            return Ok(SelfNode { login: None, tags });
        }
        let uid = me["UserID"].as_u64().ok_or(AuthError::Missing("Self.UserID"))?;
        let login = v["User"][uid.to_string()]["LoginName"].as_str().ok_or(AuthError::Missing("User.LoginName"))?;
        Ok(SelfNode { login: Some(login.to_owned()), tags })
    }

    async fn whois(&self, ip: IpAddr) -> Result<Peer, AuthError> {
        let v: serde_json::Value = serde_json::from_slice(&self.run(&["whois", "--json", &ip.to_string()]).await?)?;
        let login = v["UserProfile"]["LoginName"].as_str().ok_or(AuthError::Missing("UserProfile.LoginName"))?;
        Ok(Peer { login: login.to_owned(), tags: tags_of(&v["Node"]["Tags"]) })
    }
}

/// The logins to allow: the configured ones, else this node's owner.
pub fn effective_allowlist(configured: Vec<String>, node: Option<&SelfNode>) -> Result<Vec<String>, AuthError> {
    let mut list: Vec<String> =
        configured.into_iter().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect();
    list.sort();
    list.dedup();
    if !list.is_empty() {
        return Ok(list);
    }
    match node {
        Some(SelfNode { login: Some(l), .. }) => Ok(vec![l.to_lowercase()]),
        Some(SelfNode { tags, .. }) => Err(AuthError::TaggedNeedsAllowlist(tags.join(", "))),
        None => Ok(Vec::new()),
    }
}

pub struct Authorizer {
    ts: Tailscale,
    allow: Vec<String>,
    self_ips: Vec<IpAddr>,
    cache: Mutex<HashMap<IpAddr, (bool, Instant)>>,
}

impl Authorizer {
    pub fn new(ts: Tailscale, allow: Vec<String>, self_ips: Vec<IpAddr>) -> Self {
        Self { ts, allow, self_ips, cache: Mutex::new(HashMap::new()) }
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
        let ok = match self.ts.whois(ip).await {
            Ok(peer) => {
                let ok = peer.tags.is_empty() && self.allow.contains(&peer.login.to_lowercase());
                tracing::info!(%ip, login = peer.login, tags = ?peer.tags, allowed = ok, "tailscale whois");
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
