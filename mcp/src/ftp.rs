//! Thin synchronous wrapper around `suppaftp::FtpStream`.
//!
//! All methods are blocking and meant to be called from a `spawn_blocking`
//! context. Keeps suppaftp's API at arm's length so deploy logic doesn't
//! depend on its types directly.

use crate::config::{self, Profile};
use anyhow::{Context, Result};
use std::io::{Cursor, Read};
use suppaftp::{FtpStream, Mode};

pub struct FtpClient {
    stream: FtpStream,
}

impl FtpClient {
    pub fn connect(profile_name: &str, profile: &Profile) -> Result<Self> {
        let addr = format!("{}:{}", profile.host, profile.port);
        let mut stream = FtpStream::connect(&addr)
            .with_context(|| format!("connecting to {addr}"))?;

        if profile.passive {
            stream.set_mode(Mode::Passive);
        } else {
            stream.set_mode(Mode::Active);
        }

        let password = config::get_password(profile_name)?;
        stream
            .login(&profile.user, &password)
            .with_context(|| format!("login as {} failed", profile.user))?;

        Ok(Self { stream })
    }

    pub fn pwd(&mut self) -> Result<String> {
        self.stream.pwd().context("PWD failed")
    }

    pub fn list(&mut self, path: Option<&str>) -> Result<Vec<String>> {
        self.stream
            .list(path)
            .with_context(|| format!("LIST {} failed", path.unwrap_or(".")))
    }

    /// Upload bytes to `remote_path` (server-relative). Creates parent
    /// directories on demand.
    pub fn put_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> Result<u64> {
        self.ensure_parent_dirs(remote_path)?;
        let mut cursor = Cursor::new(bytes);
        self.stream
            .put_file(remote_path, &mut cursor)
            .with_context(|| format!("STOR {remote_path} failed"))
    }

    /// Delete a single remote file (DELE).
    pub fn delete(&mut self, remote_path: &str) -> Result<()> {
        self.stream
            .rm(remote_path)
            .with_context(|| format!("DELE {remote_path} failed"))
    }

    /// Stream upload from any `Read` source. Caller has already ensured the
    /// parent directory exists (used by deploy after a single mkdir pass).
    pub fn put_reader<R: Read>(&mut self, remote_path: &str, reader: &mut R) -> Result<u64> {
        self.stream
            .put_file(remote_path, reader)
            .with_context(|| format!("STOR {remote_path} failed"))
    }

    /// MKD each segment of `path`, ignoring "already exists" errors.
    /// Operates on the absolute `path` directly without changing CWD.
    pub fn mkdir_p(&mut self, path: &str) -> Result<()> {
        if path.is_empty() || path == "/" {
            return Ok(());
        }
        let mut acc = if path.starts_with('/') { String::from("/") } else { String::new() };
        for segment in path.trim_matches('/').split('/').filter(|s| !s.is_empty()) {
            if !acc.is_empty() && !acc.ends_with('/') {
                acc.push('/');
            }
            acc.push_str(segment);
            // suppaftp returns Err for "550 file exists" — ignore.
            let _ = self.stream.mkdir(&acc);
        }
        Ok(())
    }

    fn ensure_parent_dirs(&mut self, remote_path: &str) -> Result<()> {
        if let Some(idx) = remote_path.rfind('/') {
            let parent = &remote_path[..idx];
            if !parent.is_empty() {
                self.mkdir_p(parent)?;
            }
        }
        Ok(())
    }

    pub fn quit(mut self) {
        let _ = self.stream.quit();
    }
}
