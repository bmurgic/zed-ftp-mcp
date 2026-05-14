//! Thin synchronous wrapper around `suppaftp::FtpStream`.
//!
//! All methods are blocking and meant to be called from a `spawn_blocking`
//! context. Keeps suppaftp's API at arm's length so deploy logic doesn't
//! depend on its types directly.

use crate::config::{self, Profile};
use anyhow::{Context, Result};
use std::io::{Cursor, Read};
use suppaftp::{FtpStream, NativeTlsConnector, NativeTlsFtpStream, Mode};

enum AnyFtpStream {
    Plain(FtpStream),
    Tls(NativeTlsFtpStream),
}

/// Dispatch a `&mut self` method call to whichever stream variant is active.
macro_rules! stream {
    ($self:expr, |$s:ident| $body:expr) => {
        match &mut $self.stream {
            AnyFtpStream::Plain($s) => $body,
            AnyFtpStream::Tls($s) => $body,
        }
    };
}

pub struct FtpClient {
    stream: AnyFtpStream,
}

impl FtpClient {
    pub fn connect(profile_name: &str, profile: &Profile) -> Result<Self> {
        let addr = format!("{}:{}", profile.host, profile.port);
        let password = config::get_password(profile_name)?;

        if profile.tls {
            let tls = if profile.accept_invalid_certs {
                suppaftp::native_tls::TlsConnector::builder()
                    .danger_accept_invalid_certs(true)
                    .build()
                    .context("building TLS connector")?
            } else {
                suppaftp::native_tls::TlsConnector::new()
                    .context("building TLS connector")?
            };
            let connector = NativeTlsConnector::from(tls);
            let mut stream = NativeTlsFtpStream::connect(&addr)
                .with_context(|| format!("connecting to {addr}"))?;
            stream.set_mode(if profile.passive { Mode::Passive } else { Mode::Active });
            let mut stream = stream
                .into_secure(connector, &profile.host)
                .context("STARTTLS handshake failed")?;
            stream
                .login(&profile.user, &password)
                .with_context(|| format!("login as {} failed", profile.user))?;
            return Ok(Self { stream: AnyFtpStream::Tls(stream) });
        }

        let mut stream = FtpStream::connect(&addr)
            .with_context(|| format!("connecting to {addr}"))?;
        stream.set_mode(if profile.passive { Mode::Passive } else { Mode::Active });
        stream
            .login(&profile.user, &password)
            .with_context(|| format!("login as {} failed", profile.user))?;
        Ok(Self { stream: AnyFtpStream::Plain(stream) })
    }

    pub fn pwd(&mut self) -> Result<String> {
        stream!(self, |s| s.pwd().context("PWD failed"))
    }

    pub fn list(&mut self, path: Option<&str>) -> Result<Vec<String>> {
        stream!(self, |s| s
            .list(path)
            .with_context(|| format!("LIST {} failed", path.unwrap_or("."))))
    }

    /// Download a remote file and return its bytes.
    pub fn get_bytes(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let cursor = stream!(self, |s| s
            .retr_as_buffer(remote_path)
            .with_context(|| format!("RETR {remote_path} failed")))?;
        Ok(cursor.into_inner())
    }

    /// Upload bytes to `remote_path`. Creates parent directories on demand.
    pub fn put_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> Result<u64> {
        self.ensure_parent_dirs(remote_path)?;
        let mut cursor = Cursor::new(bytes);
        stream!(self, |s| s
            .put_file(remote_path, &mut cursor)
            .with_context(|| format!("STOR {remote_path} failed")))
    }

    /// Stream upload from any `Read` source. Caller has already ensured the
    /// parent directory exists (used by deploy after a single mkdir pass).
    pub fn put_reader<R: Read>(&mut self, remote_path: &str, reader: &mut R) -> Result<u64> {
        stream!(self, |s| s
            .put_file(remote_path, reader)
            .with_context(|| format!("STOR {remote_path} failed")))
    }

    /// MKD each segment of `path`, ignoring "already exists" errors.
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
            let _ = stream!(self, |s| s.mkdir(&acc));
        }
        Ok(())
    }

    /// Delete a single remote file (DELE).
    pub fn delete(&mut self, remote_path: &str) -> Result<()> {
        stream!(self, |s| s
            .rm(remote_path)
            .with_context(|| format!("DELE {remote_path} failed")))
    }

    /// Delete a remote directory (RMD). Directory must be empty.
    pub fn rmdir(&mut self, remote_path: &str) -> Result<()> {
        stream!(self, |s| s
            .rmdir(remote_path)
            .with_context(|| format!("RMD {remote_path} failed")))
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

    pub fn quit(self) {
        match self.stream {
            AnyFtpStream::Plain(mut s) => { let _ = s.quit(); }
            AnyFtpStream::Tls(mut s) => { let _ = s.quit(); }
        }
    }
}
