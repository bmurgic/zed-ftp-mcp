//! Thin synchronous wrapper around `suppaftp::FtpStream`.
//!
//! All methods are blocking and meant to be called from a `spawn_blocking`
//! context. Keeps suppaftp's API at arm's length so deploy logic doesn't
//! depend on its types directly.

use crate::branch_deploy::{BranchRemote, RemoteComparison, RemoteFailure, RemoteFailureKind};
use crate::config::{self, Profile};
use anyhow::{Context, Result};
use std::io::{Cursor, Read};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpStream, Mode, NativeTlsConnector, NativeTlsFtpStream};

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
                suppaftp::native_tls::TlsConnector::new().context("building TLS connector")?
            };
            let connector = NativeTlsConnector::from(tls);
            let mut stream = NativeTlsFtpStream::connect(&addr)
                .with_context(|| format!("connecting to {addr}"))?;
            stream.set_mode(if profile.passive {
                Mode::Passive
            } else {
                Mode::Active
            });
            let mut stream = stream
                .into_secure(connector, &profile.host)
                .context("STARTTLS handshake failed")?;
            stream
                .login(&profile.user, &password)
                .with_context(|| format!("login as {} failed", profile.user))?;
            return Ok(Self {
                stream: AnyFtpStream::Tls(stream),
            });
        }

        let mut stream =
            FtpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
        stream.set_mode(if profile.passive {
            Mode::Passive
        } else {
            Mode::Active
        });
        stream
            .login(&profile.user, &password)
            .with_context(|| format!("login as {} failed", profile.user))?;
        Ok(Self {
            stream: AnyFtpStream::Plain(stream),
        })
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
        let mut acc = if path.starts_with('/') {
            String::from("/")
        } else {
            String::new()
        };
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
            AnyFtpStream::Plain(mut s) => {
                let _ = s.quit();
            }
            AnyFtpStream::Tls(mut s) => {
                let _ = s.quit();
            }
        }
    }

    fn branch_set_binary_mode(&mut self) -> Result<(), FtpError> {
        stream!(self, |s| s.transfer_type(FileType::Binary))
    }

    fn branch_upload_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> Result<u64, FtpError> {
        let mut cursor = Cursor::new(bytes);
        stream!(self, |s| s.put_file(remote_path, &mut cursor))
    }

    fn branch_compare_remote_bytes(
        &mut self,
        remote_path: &str,
        expected: &[u8],
    ) -> Result<RemoteComparison, FtpError> {
        stream!(self, |s| s.retr(remote_path, |reader| {
            compare_reader_bytes(reader, expected).map_err(FtpError::ConnectionError)
        }))
    }

    #[allow(dead_code)]
    fn branch_delete_file(&mut self, remote_path: &str) -> Result<(), FtpError> {
        stream!(self, |s| s.rm(remote_path))
    }
}

impl BranchRemote for FtpClient {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure> {
        self.branch_set_binary_mode().map_err(map_branch_ftp_error)
    }

    fn mkdir_p(&mut self, path: &str) -> Result<(), RemoteFailure> {
        FtpClient::mkdir_p(self, path).map_err(|error| RemoteFailure::operation(error.to_string()))
    }

    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<u64, RemoteFailure> {
        self.branch_upload_bytes(path, bytes)
            .map_err(map_branch_ftp_error)
    }

    fn compare_remote_bytes(
        &mut self,
        path: &str,
        expected: &[u8],
    ) -> Result<RemoteComparison, RemoteFailure> {
        self.branch_compare_remote_bytes(path, expected)
            .map_err(map_branch_ftp_error)
    }

    fn delete_file(&mut self, path: &str) -> Result<(), RemoteFailure> {
        self.branch_delete_file(path).map_err(map_branch_ftp_error)
    }
}

fn map_branch_ftp_error(error: FtpError) -> RemoteFailure {
    let kind = match &error {
        FtpError::ConnectionError(_)
        | FtpError::SecureError(_)
        | FtpError::BadResponse
        | FtpError::DataConnectionAlreadyOpen => RemoteFailureKind::ConnectionLost,
        FtpError::UnexpectedResponse(_) | FtpError::InvalidAddress(_) => {
            RemoteFailureKind::Operation
        }
    };
    RemoteFailure {
        kind,
        error: error.to_string(),
    }
}

fn compare_reader_bytes(
    reader: &mut dyn Read,
    expected: &[u8],
) -> std::io::Result<RemoteComparison> {
    let mut buffer = [0; 8192];
    let mut offset = 0usize;
    let mut matches = true;
    let mut bytes_read = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes_read += read as u64;
        let end = offset.saturating_add(read);
        if end > expected.len() || expected.get(offset..end) != Some(&buffer[..read]) {
            matches = false;
        }
        offset = end;
    }
    Ok(RemoteComparison {
        matches: matches && offset == expected.len(),
        bytes_read,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        compare_reader_bytes, map_branch_ftp_error, AnyFtpStream, BranchRemote, FtpClient,
        RemoteFailureKind,
    };
    use crate::config::Profile;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::AddrParseError;
    use std::net::{TcpListener, TcpStream};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use suppaftp::types::Response;
    use suppaftp::{FtpError, Status};
    use suppaftp::{FtpStream, Mode};

    #[test]
    fn branch_adapter_classifies_suppaftp_errors() {
        let invalid_address: AddrParseError = "not-an-address"
            .parse::<std::net::SocketAddr>()
            .expect_err("fixture must not parse as an address");
        let cases = vec![
            (
                FtpError::ConnectionError(io::Error::other("lost")),
                RemoteFailureKind::ConnectionLost,
            ),
            (
                FtpError::SecureError("TLS lost".to_string()),
                RemoteFailureKind::ConnectionLost,
            ),
            (
                FtpError::UnexpectedResponse(Response::new(Status::FileUnavailable, Vec::new())),
                RemoteFailureKind::Operation,
            ),
            (FtpError::BadResponse, RemoteFailureKind::ConnectionLost),
            (
                FtpError::InvalidAddress(invalid_address),
                RemoteFailureKind::Operation,
            ),
            (
                FtpError::DataConnectionAlreadyOpen,
                RemoteFailureKind::ConnectionLost,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(map_branch_ftp_error(error).kind, expected);
        }
    }

    #[test]
    fn branch_adapter_comparison_drains_after_first_mismatch() {
        let mut reader = CountingReader {
            bytes: vec![b'x', b'b', b'c', b'd', b'e'],
            offset: 0,
            reads: 0,
        };

        let comparison = compare_reader_bytes(&mut reader, b"abcde")
            .expect("comparison should consume the reader");

        assert!(!comparison.matches);
        assert_eq!(comparison.bytes_read, 5);
        assert!(
            reader.reads >= 2,
            "reader must be polled through EOF after mismatch"
        );
    }

    struct CountingReader {
        bytes: Vec<u8>,
        offset: usize,
        reads: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            buffer[0] = self.bytes[self.offset];
            self.offset += 1;
            Ok(1)
        }
    }

    #[test]
    #[ignore]
    fn disposable_branch_round_trip() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let passive_port = reserve_port();
        let container_name = format!(
            "zed-ftp-branch-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        );
        let image = "delfer/alpine-ftp-server:latest@sha256:60bb774d8408d9d4d5c74d05d1c086a34ce192c6c1a142ffac268cac0dbc6fac";
        let output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container_name,
                "-e",
                "USERS=test|test|/home/test",
                "-e",
                "ADDRESS=127.0.0.1",
                "-e",
                &format!("MIN_PORT={passive_port}"),
                "-e",
                &format!("MAX_PORT={passive_port}"),
                "-p",
                "127.0.0.1::21",
                "-p",
                &format!("127.0.0.1:{passive_port}:{passive_port}"),
                image,
            ])
            .output()
            .expect("Docker CLI should start");
        assert!(
            output.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _container = DockerContainer {
            name: container_name,
        };
        let control_port = published_control_port(&_container.name);
        wait_for_ftp(control_port);
        let (proxy_port, commands) = start_control_proxy(control_port);

        let profile = Profile {
            host: "127.0.0.1".to_string(),
            port: proxy_port,
            user: "test".to_string(),
            remote_root: "/home/test".to_string(),
            local_root: ".".to_string(),
            passive: true,
            tls: false,
            accept_invalid_certs: false,
            ignore: Vec::new(),
        };
        let password = "test".to_string();
        let mut stream = FtpStream::connect(format!("{}:{}", profile.host, profile.port))
            .expect("FTP client should connect through the control proxy");
        stream.set_mode(Mode::Passive);
        stream
            .login(&profile.user, &password)
            .expect("FTP client should authenticate");
        let mut client = FtpClient {
            stream: AnyFtpStream::Plain(stream),
        };
        let expected = [0, b'\r', b'\n', 0xff, 0x80];
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        client
            .upload_bytes("branch-binary.bin", &expected)
            .expect("adapter should upload binary data");
        let comparison = client
            .compare_remote_bytes("branch-binary.bin", &expected)
            .expect("adapter should retrieve and compare binary data");
        assert!(comparison.matches);
        assert_eq!(comparison.bytes_read, expected.len() as u64);
        client.quit();

        let commands = commands.lock().expect("command log should not be poisoned");
        let type_index = commands
            .iter()
            .position(|command| command.starts_with("TYPE I"))
            .expect("control log should contain TYPE I");
        let stor_index = commands
            .iter()
            .position(|command| command.starts_with("STOR "))
            .expect("control log should contain STOR");
        let retr_index = commands
            .iter()
            .position(|command| command.starts_with("RETR "))
            .expect("control log should contain RETR");
        assert!(type_index < stor_index);
        assert!(type_index < retr_index);
    }

    struct DockerContainer {
        name: String,
    }

    impl Drop for DockerContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rm", "-f", &self.name])
                .output();
        }
    }

    fn reserve_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("should reserve a local port");
        listener
            .local_addr()
            .expect("reserved port should have an address")
            .port()
    }

    fn published_control_port(container_name: &str) -> u16 {
        let output = Command::new("docker")
            .args(["port", container_name, "21/tcp"])
            .output()
            .expect("Docker CLI should inspect published ports");
        assert!(output.status.success(), "docker port should succeed");
        String::from_utf8(output.stdout)
            .expect("docker port output should be UTF-8")
            .trim()
            .rsplit(':')
            .next()
            .expect("docker port output should include a port")
            .parse()
            .expect("docker port output should contain a numeric port")
    }

    fn start_control_proxy(upstream_port: u16) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("proxy should bind a control port");
        let port = listener
            .local_addr()
            .expect("proxy address should resolve")
            .port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let command_log = Arc::clone(&commands);
        thread::spawn(move || {
            let (client, _) = listener
                .accept()
                .expect("proxy should accept FTP control connection");
            let mut upstream = TcpStream::connect(("127.0.0.1", upstream_port))
                .expect("proxy should connect to FTP container");
            let mut client_reader =
                BufReader::new(client.try_clone().expect("client should clone"));
            let mut client_writer = client;
            let mut upstream_reader = upstream.try_clone().expect("upstream should clone");
            let upstream_to_client = thread::spawn(move || {
                io::copy(&mut upstream_reader, &mut client_writer)
                    .expect("proxy should forward FTP responses");
            });
            loop {
                let mut line = Vec::new();
                let read = client_reader
                    .read_until(b'\n', &mut line)
                    .expect("proxy should read FTP commands");
                if read == 0 {
                    break;
                }
                command_log
                    .lock()
                    .expect("command log should not be poisoned")
                    .push(String::from_utf8_lossy(&line).trim().to_string());
                upstream
                    .write_all(&line)
                    .expect("proxy should forward FTP commands");
            }
            let _ = upstream_to_client.join();
        });
        (port, commands)
    }

    fn wait_for_ftp(port: u16) {
        for _ in 0..40 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("FTP container did not accept a control connection");
    }
}
