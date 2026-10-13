//! Connection-profile config + OS keychain integration.
//!
//! Profiles live at `~/.config/zed-ftp/connections.toml`. Passwords never go
//! in the file — they're stored separately in the OS keychain (Keychain on
//! macOS, Secret Service on Linux, Credential Manager on Windows) under
//! service `KEYCHAIN_SERVICE` with the profile name as the account. When the
//! keychain read fails, an owner-only file at `password_file_path` is used.

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

/// Fallback password file for `profile`: `<config dir>/zed-ftp/passwords/<profile>`.
///
/// The keychain is unreachable from a process outside the user's GUI login
/// session (macOS's Background session, for example), so a password file
/// lets those processes connect. The file must be readable by its owner only.
pub fn password_file_path(profile: &str) -> Result<PathBuf> {
    if profile.is_empty() || profile.contains(['/', '\\']) || profile.starts_with('.') {
        anyhow::bail!("profile name '{profile}' cannot name a password file");
    }
    let dir = dirs::config_dir().context("could not locate user config directory")?;
    Ok(dir.join("zed-ftp").join("passwords").join(profile))
}

/// Reads a password file, refusing one that anyone but its owner can read.
/// Returns `Ok(None)` when the file does not exist.
fn read_password_file(path: &std::path::Path) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} has mode {mode:o}; run: chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let password = text.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(Some(password.to_string()))
}

/// The keychain password for `profile`, else its password file. When both
/// fail, the error names the keychain's own error and the file path.
pub fn get_password(profile: &str) -> Result<String> {
    let keychain_error = match keyring::Entry::new(KEYCHAIN_SERVICE, profile)
        .and_then(|entry| entry.get_password())
    {
        Ok(password) => return Ok(password),
        Err(e) => e,
    };
    let path = password_file_path(profile)?;
    match read_password_file(&path)? {
        Some(password) => Ok(password),
        None => anyhow::bail!(
            "no password for profile '{profile}': keychain: {keychain_error}; \
             no file at {}. Run: zed-ftp-mcp set-password {profile}, or write the \
             password to that file with mode 600",
            path.display()
        ),
    }
}

/// Whether `get_password` would find a password. A keychain error other than
/// a missing entry is returned only when no usable password file exists.
pub fn has_password(profile: &str) -> Result<bool> {
    let keychain = keyring::Entry::new(KEYCHAIN_SERVICE, profile).and_then(|e| e.get_password());
    if keychain.is_ok() {
        return Ok(true);
    }
    if read_password_file(&password_file_path(profile)?)?.is_some() {
        return Ok(true);
    }
    match keychain {
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(e.into()),
        Ok(_) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, contents: &str, mode: u32) -> PathBuf {
        let path = dir.path().join("staging");
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }

    #[test]
    fn reads_an_owner_only_file_without_its_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "s3cret\n", 0o600);
        assert_eq!(
            read_password_file(&path).unwrap(),
            Some("s3cret".to_string())
        );
    }

    #[test]
    fn a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_password_file(&dir.path().join("nope")).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_file_others_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "s3cret", 0o644);
        let err = read_password_file(&path).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
    }

    #[test]
    fn refuses_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "\n", 0o600);
        assert!(read_password_file(&path)
            .unwrap_err()
            .to_string()
            .contains("is empty"));
    }

    #[test]
    fn a_profile_name_cannot_leave_the_passwords_directory() {
        for name in ["", "../x", "a/b", ".hidden"] {
            assert!(password_file_path(name).is_err(), "{name}");
        }
        assert!(password_file_path("staging")
            .unwrap()
            .ends_with("zed-ftp/passwords/staging"));
    }
}
