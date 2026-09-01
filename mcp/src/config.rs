//! Connection-profile config + OS keychain integration.
//!
//! Profiles live at `~/.config/zed-ftp/connections.toml`. Passwords never go
//! in the file — they're stored separately in the OS keychain (Keychain on
//! macOS, Secret Service on Linux, Credential Manager on Windows) under
//! service `KEYCHAIN_SERVICE` with the profile name as the account.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const KEYCHAIN_SERVICE: &str = "zed-ftp";

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Profile {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Remote root directory that `local_root` is mirrored into.
    pub remote_root: String,
    /// Local directory to deploy. Relative paths resolve against the cwd of
    /// the agent invocation. Defaults to ".".
    #[serde(default = "default_local_root")]
    pub local_root: String,
    #[serde(default = "default_passive")]
    pub passive: bool,
    /// Use explicit FTPS (STARTTLS). Requires the server to support AUTH TLS.
    #[serde(default)]
    pub tls: bool,
    /// Skip TLS certificate verification. Only use for self-signed certs on
    /// trusted private servers.
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// Glob patterns to skip during deploy. `.gitignore` is always honored.
    #[serde(default)]
    pub ignore: Vec<String>,
}

fn default_port() -> u16 {
    21
}
fn default_local_root() -> String {
    ".".into()
}
fn default_passive() -> bool {
    true
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not locate user config directory")?;
    Ok(dir.join("zed-ftp").join("connections.toml"))
}

pub fn path_hint() -> String {
    config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.config/zed-ftp/connections.toml".to_string())
}

pub fn store_password(profile: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, profile)
        .with_context(|| format!("opening keychain entry for '{profile}'"))?;
    entry
        .set_password(password)
        .with_context(|| format!("storing password for '{profile}'"))?;
    Ok(())
}

pub fn get_password(profile: &str) -> Result<String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, profile)
        .with_context(|| format!("opening keychain entry for '{profile}'"))?;
    entry.get_password().with_context(|| {
        format!(
            "no password stored for profile '{profile}'. Run: \
             zed-ftp-mcp set-password {profile}"
        )
    })
}

pub fn has_password(profile: &str) -> Result<bool> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, profile)?;
    match entry.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
