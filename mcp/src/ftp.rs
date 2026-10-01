//! Thin synchronous wrapper around `suppaftp::FtpStream`.
//!
//! All methods are blocking and meant to be called from a `spawn_blocking`
//! context. Keeps suppaftp's API at arm's length so deploy logic doesn't
//! depend on its types directly.

use crate::branch_deploy::{BranchRemote, RemoteComparison, RemoteFailure, RemoteFailureKind};
use crate::config::{self, Profile};
use crate::deploy::DeployRemote;
use crate::drift::DriftRemote;
use crate::remote_path::split_parent;
use anyhow::{Context, Result};
use std::io::{Cursor, Read};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpStream, Mode, NativeTlsConnector, NativeTlsFtpStream, Status};

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

    /// Download `remote_path`, or return `None` when the file is missing.
    ///
    /// A 550 reply also covers a denied read and a non-file target, so only a 550 whose parent
    /// directory listing lacks the file name means missing. A 550 with the name listed returns
    /// the original 550 error. A failed listing returns the listing's error. Every other error
    /// passes through unchanged. Many servers, vsftpd among them, leave dotfiles out of a plain
    /// listing, so a dotfile also needs a listing with hidden files before it counts as missing.
    pub fn download_or_missing(&mut self, remote_path: &str) -> Result<Option<Vec<u8>>, FtpError> {
        let retrieve_error = match stream!(self, |s| s.retr_as_buffer(remote_path)) {
            Ok(cursor) => return Ok(Some(cursor.into_inner())),
            Err(error) => error,
        };
        if !is_file_unavailable(&retrieve_error) {
            return Err(retrieve_error);
        }
        if self.is_listed_in_parent(remote_path)? {
            Err(retrieve_error)
        } else {
            Ok(None)
        }
    }

    /// Lists the parent directory of `remote_path` and reports whether the listing names the file.
    /// A dotfile the plain listing omits gets a second listing, `NLST -a`, which includes hidden
    /// files.
    fn is_listed_in_parent(&mut self, remote_path: &str) -> Result<bool, FtpError> {
        // A `None` parent lists the working directory, where a bare file name lives.
        let (parent, file_name) = split_parent(remote_path);
        let listing = stream!(self, |s| s.nlst(parent))?;
        if is_named_in(&listing, file_name) {
            return Ok(true);
        }
        if !file_name.starts_with('.') {
            return Ok(false);
        }
        let hidden_files_argument = match parent {
            Some(parent) => format!("-a {parent}"),
            None => "-a".to_string(),
        };
        let listing = stream!(self, |s| s.nlst(Some(&hidden_files_argument)))?;
        Ok(is_named_in(&listing, file_name))
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

    fn branch_mkdir_p(&mut self, path: &str) -> Result<(), FtpError> {
        branch_mkdir_path(path, |directory| stream!(self, |s| s.mkdir(directory)))
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

    fn branch_delete_file(&mut self, remote_path: &str) -> Result<(), FtpError> {
        stream!(self, |s| s.rm(remote_path))
    }
}

impl BranchRemote for FtpClient {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure> {
        self.branch_set_binary_mode().map_err(map_branch_ftp_error)
    }

    fn mkdir_p(&mut self, path: &str) -> Result<(), RemoteFailure> {
        self.branch_mkdir_p(path).map_err(map_branch_ftp_error)
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

    fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure> {
        self.download_or_missing(path).map_err(map_branch_ftp_error)
    }
}

impl DeployRemote for FtpClient {
    fn mkdir_p(&mut self, path: &str) -> Result<()> {
        FtpClient::mkdir_p(self, path)
    }

    fn put_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> Result<u64> {
        FtpClient::put_bytes(self, remote_path, bytes)
    }

    fn put_reader<R: Read>(&mut self, remote_path: &str, reader: &mut R) -> Result<u64> {
        FtpClient::put_reader(self, remote_path, reader)
    }

    fn quit(self) {
        FtpClient::quit(self)
    }
}

impl DriftRemote for FtpClient {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure> {
        self.branch_set_binary_mode().map_err(map_branch_ftp_error)
    }

    fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure> {
        self.download_or_missing(path).map_err(map_branch_ftp_error)
    }
}

fn is_file_unavailable(error: &FtpError) -> bool {
    matches!(
        error,
        FtpError::UnexpectedResponse(response) if response.status == Status::FileUnavailable
    )
}

fn is_named_in(listing: &[String], file_name: &str) -> bool {
    listing
        .iter()
        .any(|entry| listing_entry_name(entry) == file_name)
}

/// `NLST` servers return bare names or full paths, so compare on the last component. Only line
/// endings are stripped, because spaces at either end belong to the file name.
fn listing_entry_name(entry: &str) -> &str {
    let entry = entry.trim_end_matches(['\r', '\n']);
    entry.rsplit_once('/').map_or(entry, |(_, name)| name)
}

fn map_branch_ftp_error(error: FtpError) -> RemoteFailure {
    match &error {
        FtpError::ConnectionError(_)
        | FtpError::SecureError(_)
        | FtpError::BadResponse
        | FtpError::DataConnectionAlreadyOpen => RemoteFailure {
            kind: RemoteFailureKind::ConnectionLost,
            error: error.to_string(),
        },
        // FTP 421 closes the control connection, so deletion must stop rather than issue another command.
        FtpError::UnexpectedResponse(response) if response.status == Status::NotAvailable => {
            RemoteFailure {
                kind: RemoteFailureKind::ConnectionLost,
                error: error.to_string(),
            }
        }
        FtpError::UnexpectedResponse(_) | FtpError::InvalidAddress(_) => {
            RemoteFailure::operation(error.to_string())
        }
    }
}

fn branch_mkdir_path(
    path: &str,
    mut mkdir: impl FnMut(&str) -> Result<(), FtpError>,
) -> Result<(), FtpError> {
    if path.is_empty() || path == "/" {
        return Ok(());
    }
    let mut directory = if path.starts_with('/') {
        String::from("/")
    } else {
        String::new()
    };
    for segment in path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        if !directory.is_empty() && !directory.ends_with('/') {
            directory.push('/');
        }
        directory.push_str(segment);
        if let Err(error) = mkdir(&directory) {
            if !is_directory_already_exists(&error) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn is_directory_already_exists(error: &FtpError) -> bool {
    let FtpError::UnexpectedResponse(response) = error else {
        return false;
    };
    if response.status != Status::FileUnavailable {
        return false;
    }
    // Servers word this differently ("550 Directory already exists",
    // "550 File or directory already exists"), so match the phrase, not the line.
    std::str::from_utf8(&response.body)
        .is_ok_and(|body| body.to_ascii_lowercase().contains("already exists"))
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
        compare_reader_bytes, map_branch_ftp_error, AnyFtpStream, BranchRemote, DeployRemote,
        FtpClient, RemoteFailure, RemoteFailureKind,
    };
    use crate::branch_deploy::{
        execute_deletion, preview_merge, BlobSource, BranchDeletePlan, BranchDeployError,
        BranchDeployPlan, DeletePathResult, DeletePathStatus, DeployMode, MergeStatus,
        PlannedUpload, UploadStatus,
    };
    use crate::deploy::{deploy, upload_file, UploadFileRequest};
    use crate::drift::tests::TestRepo;
    use crate::drift::{
        check_drift, resolve_expect_ref, validate_expected_paths, DriftReason, DriftTarget,
        DriftedFile,
    };
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::AddrParseError;
    use std::net::{TcpListener, TcpStream};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use suppaftp::types::Response;
    use suppaftp::{FtpError, Status};
    use suppaftp::{FtpStream, Mode};

    type Deletion421Server = (Vec<Vec<u8>>, usize);

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
            (
                FtpError::UnexpectedResponse(Response::new(Status::BadFilename, Vec::new())),
                RemoteFailureKind::Operation,
            ),
            (
                FtpError::UnexpectedResponse(Response::new(Status::NotAvailable, Vec::new())),
                RemoteFailureKind::ConnectionLost,
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
    fn deletion_executor_stops_when_ftp_adapter_receives_421() {
        let (mut client, server) = branch_client_for_deletion_421();
        let manifest = execute_deletion(
            BranchDeletePlan {
                profile: "test".to_string(),
                repository_root: "/approved/repository".to_string(),
                base_commit: "a".repeat(40),
                head_commit: "b".repeat(40),
                reason: "remove approved files".to_string(),
                dry_run: false,
                paths: vec![
                    DeletePathResult {
                        git_path: "first.txt".to_string(),
                        remote_path: "first.txt".to_string(),
                        status: DeletePathStatus::Planned,
                    },
                    DeletePathResult {
                        git_path: "second.txt".to_string(),
                        remote_path: "second.txt".to_string(),
                        status: DeletePathStatus::Planned,
                    },
                ],
                blocked: Vec::new(),
                failures: Vec::new(),
            },
            &mut client,
        );

        assert!(!manifest.success);
        assert_eq!(manifest.paths[0].status, DeletePathStatus::Failed);
        assert_eq!(manifest.paths[1].status, DeletePathStatus::NotAttempted);

        let (commands, connections) = server.join().expect("FTP server should complete");
        assert_eq!(connections, 1, "deletion must not reconnect after FTP 421");
        assert_eq!(
            commands,
            vec![b"TYPE I\r\n".to_vec(), b"DELE first.txt\r\n".to_vec()],
            "deletion must not send a second DELE after FTP 421"
        );
    }

    #[test]
    fn branch_adapter_mkdir_reports_operation_failure() {
        let (mut client, server) =
            branch_client_for_mkd_response(Some(b"553 invalid directory\r\n"));

        let failure = BranchRemote::mkdir_p(&mut client, "/remote")
            .expect_err("branch mkdir should report an operation failure");

        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        assert_eq!(
            server.join().expect("MKD server should complete"),
            b"MKD /remote\r\n"
        );
    }

    #[test]
    fn branch_adapter_mkdir_reports_permission_denied_550_as_operation_failure() {
        let (mut client, server) =
            branch_client_for_mkd_response(Some(b"550 permission denied\r\n"));

        let failure = BranchRemote::mkdir_p(&mut client, "/remote")
            .expect_err("branch mkdir should report permission denied");

        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        assert_eq!(
            server.join().expect("MKD server should complete"),
            b"MKD /remote\r\n"
        );
    }

    #[test]
    fn branch_adapter_mkdir_reports_connection_loss() {
        let (mut client, server) = branch_client_for_mkd_response(None);

        let failure = BranchRemote::mkdir_p(&mut client, "/remote")
            .expect_err("branch mkdir should report a lost connection");

        assert_eq!(failure.kind, RemoteFailureKind::ConnectionLost);
        assert_eq!(
            server.join().expect("MKD server should complete"),
            b"MKD /remote\r\n"
        );
    }

    #[test]
    fn branch_adapter_mkdir_ignores_directory_already_exists() {
        let (mut client, server) =
            branch_client_for_mkd_response(Some(b"550 Directory Already Exists\r\n"));

        BranchRemote::mkdir_p(&mut client, "/remote")
            .expect("branch mkdir should ignore an existing directory");

        assert_eq!(
            server.join().expect("MKD server should complete"),
            b"MKD /remote\r\n"
        );
    }

    #[test]
    fn branch_adapter_mkdir_ignores_file_or_directory_already_exists() {
        let (mut client, server) =
            branch_client_for_mkd_response(Some(b"550 File or directory already exists\r\n"));

        BranchRemote::mkdir_p(&mut client, "/remote")
            .expect("branch mkdir should ignore an existing directory");

        assert_eq!(
            server.join().expect("MKD server should complete"),
            b"MKD /remote\r\n"
        );
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

    fn branch_client_for_mkd_response(
        response: Option<&'static [u8]>,
    ) -> (FtpClient, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("MKD server should bind");
        let address = listener
            .local_addr()
            .expect("MKD server address should resolve");
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("MKD server should accept");
            connection
                .write_all(b"220 ready\r\n")
                .expect("MKD server should send greeting");
            let mut command = Vec::new();
            BufReader::new(
                connection
                    .try_clone()
                    .expect("MKD server connection should clone"),
            )
            .read_until(b'\n', &mut command)
            .expect("MKD server should receive command");
            if let Some(response) = response {
                connection
                    .write_all(response)
                    .expect("MKD server should send response");
            }
            command
        });
        let stream =
            FtpStream::connect(address.to_string()).expect("client should connect to MKD server");

        (
            FtpClient {
                stream: AnyFtpStream::Plain(stream),
            },
            server,
        )
    }

    #[test]
    fn branch_adapter_download_returns_the_file_bytes_without_listing() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Data(vec![0, b'\r', b'\n', 0xff]),
            FakeAnswer::Line("500 no listing expected"),
        );

        let downloaded = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect("a readable file should download");

        assert_eq!(downloaded, Some(vec![0, b'\r', b'\n', 0xff]));
        client.quit();
        let commands = server.join().expect("download server should complete");
        assert!(commands.iter().all(|command| !command.starts_with("NLST")));
    }

    #[test]
    fn branch_adapter_download_treats_550_with_absent_name_in_parent_listing_as_missing() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"other.txt\r\nxa.txt\r\n".to_vec()),
        );

        let downloaded = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect("an absent file is not a failure");

        assert_eq!(
            downloaded, None,
            "a listing entry that only ends with the name is not the file"
        );
        client.quit();
        let commands = server.join().expect("download server should complete");
        assert!(commands.contains(&"NLST /site/dir".to_string()));
        assert!(
            commands
                .iter()
                .all(|command| !command.starts_with("NLST -a")),
            "only a dotfile needs the hidden-file listing: {commands:?}"
        );
    }

    #[test]
    fn branch_adapter_download_reports_550_with_name_in_parent_listing_as_operation_failure() {
        for listing in [&b"a.txt\r\nb.txt\r\n"[..], b"/site/dir/a.txt\r\n"] {
            let (mut client, server) = branch_client_for_download(
                FakeAnswer::Line("550 Failed to open file."),
                FakeAnswer::Data(listing.to_vec()),
            );

            let failure = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
                .expect_err("an existing file that cannot be read is a failure");

            assert_eq!(failure.kind, RemoteFailureKind::Operation);
            client.quit();
            server.join().expect("download server should complete");
        }
    }

    #[test]
    fn branch_adapter_download_reports_550_with_failed_listing_as_operation_failure() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Line("550 Permission denied."),
        );

        let failure = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect_err("an unverifiable 550 must not read as missing");

        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        client.quit();
        server.join().expect("download server should complete");
    }

    #[test]
    fn branch_adapter_download_reports_connection_loss_during_the_listing() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Drop,
        );

        let failure = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect_err("a lost connection must not read as missing");

        assert_eq!(failure.kind, RemoteFailureKind::ConnectionLost);
        client.quit();
        server.join().expect("download server should complete");
    }

    #[test]
    fn branch_adapter_download_reports_other_replies_without_listing() {
        for (reply, expected_kind) in [
            (
                "451 Local error in processing.",
                RemoteFailureKind::Operation,
            ),
            (
                "421 Service not available.",
                RemoteFailureKind::ConnectionLost,
            ),
        ] {
            let (mut client, server) = branch_client_for_download(
                FakeAnswer::Line(reply),
                FakeAnswer::Data(b"other.txt\r\n".to_vec()),
            );

            let failure = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
                .expect_err("only a 550 reply can mean missing");

            assert_eq!(failure.kind, expected_kind, "{reply}");
            client.quit();
            let commands = server.join().expect("download server should complete");
            assert!(
                commands.iter().all(|command| !command.starts_with("NLST")),
                "{reply} must not trigger a parent listing"
            );
        }
    }

    #[test]
    fn branch_adapter_download_reports_a_dropped_control_connection() {
        let (mut client, server) =
            branch_client_for_download(FakeAnswer::Drop, FakeAnswer::Line("500 unused"));

        let failure = BranchRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect_err("a dropped connection is a failure");

        assert_eq!(failure.kind, RemoteFailureKind::ConnectionLost);
        client.quit();
        server.join().expect("download server should complete");
    }

    #[test]
    fn branch_adapter_download_lists_the_root_and_the_working_directory_for_shallow_paths() {
        for (remote_path, expected_listing_command) in [("/a.txt", "NLST /"), ("a.txt", "NLST")] {
            let (mut client, server) = branch_client_for_download(
                FakeAnswer::Line("550 Failed to open file."),
                FakeAnswer::Data(b"other.txt\r\n".to_vec()),
            );

            let downloaded = BranchRemote::download_bytes(&mut client, remote_path)
                .expect("an absent file is not a failure");

            assert_eq!(downloaded, None);
            client.quit();
            let commands = server.join().expect("download server should complete");
            assert!(
                commands.contains(&expected_listing_command.to_string()),
                "{remote_path} should list with {expected_listing_command}: {commands:?}"
            );
        }
    }

    #[test]
    fn branch_adapter_download_reports_a_550_dotfile_the_plain_listing_hides_as_operation_failure()
    {
        let (mut client, server) = scripted_download_client(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"index.php\r\n".to_vec()),
            FakeAnswer::Data(b".\r\n..\r\n.htaccess\r\nindex.php\r\n".to_vec()),
        );

        let failure = BranchRemote::download_bytes(&mut client, "/site/dir/.htaccess")
            .expect_err("an existing dotfile that cannot be read is a failure");

        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        client.quit();
        let commands = server.join().expect("download server should complete");
        assert!(
            commands.contains(&"NLST -a /site/dir".to_string()),
            "{commands:?}"
        );
    }

    #[test]
    fn branch_adapter_download_treats_a_550_dotfile_absent_from_both_listings_as_missing() {
        let (mut client, server) = scripted_download_client(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"index.php\r\n".to_vec()),
            FakeAnswer::Data(b".\r\n..\r\nindex.php\r\n".to_vec()),
        );

        let downloaded = BranchRemote::download_bytes(&mut client, "/site/dir/.htaccess")
            .expect("an absent dotfile is not a failure");

        assert_eq!(downloaded, None);
        client.quit();
        server.join().expect("download server should complete");
    }

    #[test]
    fn branch_adapter_download_reports_a_550_dotfile_with_a_failed_hidden_listing_as_failure() {
        let (mut client, server) = scripted_download_client(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"index.php\r\n".to_vec()),
            FakeAnswer::Line("501 Unknown option."),
        );

        let failure = BranchRemote::download_bytes(&mut client, "/site/dir/.htaccess")
            .expect_err("an unverifiable dotfile 550 must not read as missing");

        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        client.quit();
        server.join().expect("download server should complete");
    }

    #[test]
    fn branch_adapter_download_matches_a_listed_name_with_its_spaces() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b" notes.txt\r\nreport.txt \r\n".to_vec()),
        );

        let failure = BranchRemote::download_bytes(&mut client, "/site/dir/ notes.txt")
            .expect_err("a listed file that cannot be read is a failure");

        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        client.quit();
        server.join().expect("download server should complete");
    }

    // `DriftRemote` is called by full path: `BranchRemote` has the same method names, so
    // importing both makes a bare call ambiguous.
    #[test]
    fn drift_adapter_download_follows_the_550_plus_listing_rule() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"other.txt\r\n".to_vec()),
        );
        let missing = crate::drift::DriftRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect("an absent file is not a failure");
        assert_eq!(missing, None);
        client.quit();
        server.join().expect("download server should complete");

        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"a.txt\r\n".to_vec()),
        );
        let failure = crate::drift::DriftRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect_err("a listed file that cannot be read is a failure");
        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        client.quit();
        server.join().expect("download server should complete");

        let (mut client, server) = scripted_download_client(
            FakeAnswer::Line("550 Failed to open file."),
            FakeAnswer::Data(b"index.php\r\n".to_vec()),
            FakeAnswer::Data(b".htaccess\r\n".to_vec()),
        );
        let failure = crate::drift::DriftRemote::download_bytes(&mut client, "/site/dir/.htaccess")
            .expect_err("a dotfile only the hidden-file listing shows is a failure");
        assert_eq!(failure.kind, RemoteFailureKind::Operation);
        client.quit();
        server.join().expect("download server should complete");
    }

    #[test]
    fn deploy_adapter_creates_directories_uploads_and_quits_through_the_ftp_client() {
        let (mut client, server) = branch_client_for_download(
            FakeAnswer::Line("550 unused"),
            FakeAnswer::Line("550 unused"),
        );

        DeployRemote::mkdir_p(&mut client, "/site/dir").expect("mkdir should succeed");
        let sent = DeployRemote::put_bytes(&mut client, "/site/dir/a.bin", &[0, b'\r', b'\n'])
            .expect("put_bytes should upload");
        let mut reader = io::Cursor::new(b"hello".to_vec());
        let streamed = DeployRemote::put_reader(&mut client, "/site/dir/b.txt", &mut reader)
            .expect("put_reader should upload");
        DeployRemote::quit(client);

        assert_eq!(sent, 3);
        assert_eq!(streamed, 5);
        // put_bytes creates the parent directories again; put_reader does not.
        assert_eq!(
            server.join().expect("download server should complete"),
            [
                "MKD /site",
                "MKD /site/dir",
                "MKD /site",
                "MKD /site/dir",
                "PASV",
                "STOR /site/dir/a.bin",
                "received 3 bytes",
                "PASV",
                "STOR /site/dir/b.txt",
                "received 5 bytes",
                "QUIT",
            ]
        );
    }

    #[test]
    fn drift_adapter_reports_a_lost_connection_and_selects_binary_mode() {
        let (mut client, server) =
            branch_client_for_download(FakeAnswer::Drop, FakeAnswer::Line("500 unused"));

        crate::drift::DriftRemote::set_binary_mode(&mut client).expect("the server accepts TYPE I");
        let failure = crate::drift::DriftRemote::download_bytes(&mut client, "/site/dir/a.txt")
            .expect_err("a dropped connection is a failure");

        assert_eq!(failure.kind, RemoteFailureKind::ConnectionLost);
        client.quit();
        let commands = server.join().expect("download server should complete");
        assert_eq!(commands.first().map(String::as_str), Some("TYPE I"));
    }

    enum FakeAnswer {
        Data(Vec<u8>),
        Line(&'static str),
        Drop,
    }

    /// A scripted FTP server that answers `RETR` and `NLST` with the given answers over a
    /// passive data connection, and returns every control command it received. It refuses
    /// `NLST -a`, which only a dotfile probe sends.
    fn branch_client_for_download(
        retr_answer: FakeAnswer,
        nlst_answer: FakeAnswer,
    ) -> (FtpClient, thread::JoinHandle<Vec<String>>) {
        scripted_download_client(
            retr_answer,
            nlst_answer,
            FakeAnswer::Line("500 no hidden-file listing expected"),
        )
    }

    /// Like `branch_client_for_download`, but `NLST -a` gets `hidden_nlst_answer`.
    fn scripted_download_client(
        retr_answer: FakeAnswer,
        nlst_answer: FakeAnswer,
        hidden_nlst_answer: FakeAnswer,
    ) -> (FtpClient, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("download server should bind");
        let address = listener
            .local_addr()
            .expect("download server address should resolve");
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("download server should accept");
            connection
                .write_all(b"220 ready\r\n")
                .expect("download server should send greeting");
            let mut reader = BufReader::new(
                connection
                    .try_clone()
                    .expect("download server connection should clone"),
            );
            let mut commands = Vec::new();
            let mut data_listener: Option<TcpListener> = None;
            loop {
                let mut line = String::new();
                if reader
                    .read_line(&mut line)
                    .expect("download server should read a command")
                    == 0
                {
                    break;
                }
                let command = line.trim().to_string();
                let verb = command.split(' ').next().unwrap_or_default().to_string();
                let is_hidden_file_listing = command.starts_with("NLST -a");
                commands.push(command);
                match verb.as_str() {
                    "PASV" => {
                        let passive =
                            TcpListener::bind("127.0.0.1:0").expect("data listener should bind");
                        let port = passive.local_addr().expect("data address").port();
                        write!(
                            connection,
                            "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                            port / 256,
                            port % 256
                        )
                        .expect("download server should answer PASV");
                        data_listener = Some(passive);
                    }
                    "RETR" | "NLST" => {
                        let answer = if verb == "RETR" {
                            &retr_answer
                        } else if is_hidden_file_listing {
                            &hidden_nlst_answer
                        } else {
                            &nlst_answer
                        };
                        match answer {
                            FakeAnswer::Data(bytes) => {
                                connection
                                    .write_all(b"150 Opening data connection\r\n")
                                    .expect("download server should open the transfer");
                                let (mut data, _) = data_listener
                                    .take()
                                    .expect("PASV should precede the transfer")
                                    .accept()
                                    .expect("data connection should arrive");
                                data.write_all(bytes)
                                    .expect("download server should send data");
                                drop(data);
                                connection
                                    .write_all(b"226 Transfer complete\r\n")
                                    .expect("download server should finish the transfer");
                            }
                            FakeAnswer::Line(reply) => {
                                write!(connection, "{reply}\r\n")
                                    .expect("download server should send its reply");
                            }
                            FakeAnswer::Drop => break,
                        }
                    }
                    "STOR" => {
                        connection
                            .write_all(b"150 Opening data connection\r\n")
                            .expect("download server should open the upload");
                        let (mut data, _) = data_listener
                            .take()
                            .expect("PASV should precede the upload")
                            .accept()
                            .expect("data connection should arrive");
                        let mut received = Vec::new();
                        data.read_to_end(&mut received)
                            .expect("download server should receive the upload");
                        commands.push(format!("received {} bytes", received.len()));
                        connection
                            .write_all(b"226 Transfer complete\r\n")
                            .expect("download server should finish the upload");
                    }
                    "QUIT" => {
                        let _ = connection.write_all(b"221 bye\r\n");
                        break;
                    }
                    _ => {
                        connection
                            .write_all(b"200 ok\r\n")
                            .expect("download server should acknowledge");
                    }
                }
            }
            commands
        });
        let mut stream = FtpStream::connect(address.to_string())
            .expect("client should connect to download server");
        stream.set_mode(Mode::Passive);

        (
            FtpClient {
                stream: AnyFtpStream::Plain(stream),
            },
            server,
        )
    }

    fn branch_client_for_deletion_421() -> (FtpClient, thread::JoinHandle<Deletion421Server>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("deletion server should bind");
        let address = listener
            .local_addr()
            .expect("deletion server address should resolve");
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("deletion server should accept");
            connection
                .write_all(b"220 ready\r\n")
                .expect("deletion server should send greeting");
            let reader_connection = connection
                .try_clone()
                .expect("deletion server connection should clone");
            let mut reader = BufReader::new(reader_connection);
            let mut commands = Vec::new();

            for response in [
                b"200 binary\r\n".as_slice(),
                b"421 closing control connection\r\n",
            ] {
                let mut command = Vec::new();
                reader
                    .read_until(b'\n', &mut command)
                    .expect("deletion server should receive command");
                commands.push(command);
                connection
                    .write_all(response)
                    .expect("deletion server should send response");
            }

            connection
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("deletion server should set command timeout");
            let mut unexpected_command = Vec::new();
            match reader.read_until(b'\n', &mut unexpected_command) {
                Ok(0) => {}
                Ok(_) => commands.push(unexpected_command),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => panic!("deletion server failed reading command: {error}"),
            }

            listener
                .set_nonblocking(true)
                .expect("deletion server should make listener nonblocking");
            let deadline = Instant::now() + Duration::from_millis(250);
            let mut connections = 1;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((_connection, _)) => connections += 1,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("deletion server failed accepting connection: {error}"),
                }
            }
            (commands, connections)
        });
        let stream = FtpStream::connect(address.to_string())
            .expect("client should connect to deletion server");

        (
            FtpClient {
                stream: AnyFtpStream::Plain(stream),
            },
            server,
        )
    }

    #[test]
    #[ignore]
    fn disposable_branch_round_trip() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let (_container, mut client, commands) = disposable_branch_client("round-trip");
        let expected = [0, b'\r', b'\n', 0xff, 0x80];
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        client
            .mkdir_p("branch-round-trip")
            .expect("adapter should create the parent directory");
        client
            .mkdir_p("branch-round-trip")
            .expect("adapter should accept the existing parent directory");
        client
            .upload_bytes("branch-round-trip/branch-binary.bin", &expected)
            .expect("adapter should upload binary data");
        let comparison = client
            .compare_remote_bytes("branch-round-trip/branch-binary.bin", &expected)
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

    #[test]
    #[ignore]
    fn disposable_branch_deletion() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let (_container, mut client, commands) = disposable_branch_client("delete");
        BranchRemote::set_binary_mode(&mut client).expect("adapter should select binary mode");
        BranchRemote::mkdir_p(&mut client, "branch-deletion")
            .expect("adapter should create the test parent");
        BranchRemote::upload_bytes(&mut client, "branch-deletion/remove.bin", &[0, 0xff])
            .expect("adapter should seed the requested file");
        BranchRemote::upload_bytes(&mut client, "branch-deletion/keep.bin", &[0x80, 1])
            .expect("adapter should seed the unrequested file");

        commands
            .lock()
            .expect("command log should not be poisoned")
            .clear();
        let manifest = execute_deletion(
            BranchDeletePlan {
                profile: "disposable".to_string(),
                repository_root: "/approved/repository".to_string(),
                base_commit: "a".repeat(40),
                head_commit: "b".repeat(40),
                reason: "remove the approved test file".to_string(),
                dry_run: false,
                paths: vec![DeletePathResult {
                    git_path: "branch-deletion/remove.bin".to_string(),
                    remote_path: "branch-deletion/remove.bin".to_string(),
                    status: DeletePathStatus::Planned,
                }],
                blocked: Vec::new(),
                failures: Vec::new(),
            },
            &mut client,
        );

        assert!(manifest.success);
        assert!(!manifest.dry_run);
        assert_eq!(manifest.counts.planned, 0);
        assert_eq!(manifest.counts.deleted, 1);
        assert_eq!(manifest.counts.failed, 0);
        assert_eq!(manifest.counts.not_attempted, 0);
        assert_eq!(manifest.counts.blocked, 0);
        assert_eq!(manifest.paths.len(), 1);
        assert_eq!(manifest.paths[0].status, DeletePathStatus::Deleted);
        let commands = commands.lock().expect("command log should not be poisoned");
        let type_index = commands
            .iter()
            .position(|command| command.starts_with("TYPE I"))
            .expect("deletion executor should select binary mode");
        let delete_index = commands
            .iter()
            .position(|command| command == "DELE branch-deletion/remove.bin")
            .expect("deletion executor should delete the approved path");
        assert!(type_index < delete_index);
        drop(commands);
        assert!(
            client.get_bytes("branch-deletion/remove.bin").is_err(),
            "the requested file must be absent"
        );
        assert_eq!(
            client
                .get_bytes("branch-deletion/keep.bin")
                .expect("the unrequested file must remain"),
            vec![0x80, 1]
        );
        client.quit();
    }

    #[test]
    #[ignore]
    fn disposable_branch_download() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let (_container, mut client, commands) = disposable_branch_client("download");
        let expected = [0, b'\r', b'\n', 0xff, 0x80];
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        client
            .mkdir_p("branch-download/subdirectory")
            .expect("adapter should create the test directories");
        client
            .upload_bytes("branch-download/present.bin", &expected)
            .expect("adapter should seed the present file");

        assert_eq!(
            client
                .download_bytes("branch-download/present.bin")
                .expect("a present file should download"),
            Some(expected.to_vec())
        );
        assert_eq!(
            client
                .download_bytes("branch-download/absent.bin")
                .expect("an absent file in an existing directory is missing, not a failure"),
            None
        );
        // vsftpd lists a directory that does not exist as empty, so a path in a new directory is missing.
        assert_eq!(
            client
                .download_bytes("branch-download/new-directory/x.bin")
                .expect("an absent directory lists as empty on this server"),
            None
        );
        let unreadable = client
            .download_bytes("branch-download/subdirectory")
            .expect_err("a directory cannot be downloaded, and its name is listed");
        assert_eq!(unreadable.kind, RemoteFailureKind::Operation);

        let commands = commands.lock().expect("command log should not be poisoned");
        let type_index = commands
            .iter()
            .position(|command| command.starts_with("TYPE I"))
            .expect("control log should contain TYPE I");
        let first_retr_index = commands
            .iter()
            .position(|command| command.starts_with("RETR "))
            .expect("control log should contain RETR");
        assert!(type_index < first_retr_index);
        assert!(
            commands.contains(&"NLST branch-download".to_string()),
            "a 550 reply must be confirmed with a parent listing: {commands:?}"
        );
        drop(commands);
        client.quit();
    }

    /// Serves fixed blobs by object ID, so the preview test needs no Git repository.
    struct FixedBlobs(std::collections::BTreeMap<&'static str, &'static [u8]>);

    impl BlobSource for FixedBlobs {
        fn read_blob(&mut self, object_id: &str) -> Result<Vec<u8>, BranchDeployError> {
            self.0
                .get(object_id)
                .map(|blob| blob.to_vec())
                .ok_or_else(|| BranchDeployError::Other(anyhow::anyhow!("no blob {object_id}")))
        }
    }

    #[test]
    #[ignore]
    fn disposable_merge_preview_changes_nothing_on_the_server() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let base = b"one\ntwo\nthree\n";
        let head = b"one\nTWO head\nthree\n";
        let conflicting_server = b"one\nTWO server\nthree\n";
        let (_container, mut client, commands) = disposable_branch_client("merge-preview");
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        // The pinned image fails MKD on an existing directory, so the files sit in the home directory.
        client
            .upload_bytes("preview-a.txt", base)
            .expect("adapter should seed the fast-forward file");
        client
            .upload_bytes("preview-b.txt", conflicting_server)
            .expect("adapter should seed the conflicting file");

        let mut plan = BranchDeployPlan::empty("disposable", "/repo");
        plan.mode = DeployMode::Merge;
        plan.uploads = ["preview-a.txt", "preview-b.txt"]
            .into_iter()
            .map(|path| PlannedUpload {
                git_path: path.to_string(),
                remote_path: path.to_string(),
                object_id: "head".to_string(),
                bytes: head.len() as u64,
                base_object_id: Some("base".to_string()),
            })
            .collect();
        plan.touched_paths = plan.uploads.len();
        let mut blobs = FixedBlobs(std::collections::BTreeMap::from([
            ("head", head.as_slice()),
            ("base", base.as_slice()),
        ]));
        commands
            .lock()
            .expect("command log should not be poisoned")
            .clear();

        let manifest = preview_merge(plan, true, &mut blobs, &mut client);

        assert!(manifest.dry_run);
        assert!(manifest.blocked_by_conflicts);
        assert!(!manifest.success);
        assert_eq!(
            manifest.uploads[0].merge_status,
            Some(MergeStatus::FastForward)
        );
        assert_eq!(manifest.uploads[0].upload_status, UploadStatus::Planned);
        assert_eq!(
            manifest.uploads[1].merge_status,
            Some(MergeStatus::Conflict)
        );
        {
            let commands = commands.lock().expect("command log should not be poisoned");
            let type_index = commands
                .iter()
                .position(|command| command.starts_with("TYPE I"))
                .expect("a preview should select binary mode");
            let first_retr_index = commands
                .iter()
                .position(|command| command.starts_with("RETR "))
                .expect("a preview should download server copies");
            assert!(type_index < first_retr_index);
            let writes: Vec<&String> = commands
                .iter()
                .filter(|command| {
                    [
                        "STOR ", "STOU", "APPE ", "MKD ", "DELE ", "RMD ", "RNFR ", "RNTO ",
                    ]
                    .iter()
                    .any(|write| command.starts_with(write))
                })
                .collect();
            assert!(writes.is_empty(), "a preview must not write: {writes:?}");
        }
        assert_eq!(
            client
                .get_bytes("preview-a.txt")
                .expect("the fast-forward file must remain"),
            base.to_vec()
        );
        assert_eq!(
            client
                .get_bytes("preview-b.txt")
                .expect("the conflicting file must remain"),
            conflicting_server.to_vec()
        );
        client.quit();
    }

    #[test]
    #[ignore]
    fn disposable_drift_check_classifies_server_copies_and_writes_nothing() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let repo = TestRepo::new();
        for name in ["clean.bin", "upload-match.bin", "edited.bin", "gone.bin"] {
            repo.write(name, &[b'e', 0, b'\r', b'\n', 0xff]);
        }
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("HEAD should resolve");
        let (_container, mut client, commands) = disposable_branch_client("drift");
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        // The pinned image fails MKD on an existing directory, so the files sit in the home directory.
        for (name, bytes) in [
            ("clean.bin", &[b'e', 0, b'\r', b'\n', 0xff][..]),
            ("upload-match.bin", &[b'u', 0, 0x80][..]),
            ("edited.bin", &[b's', 0, 0x80][..]),
        ] {
            client
                .upload_bytes(name, bytes)
                .expect("adapter should seed the server copy");
        }
        let targets: Vec<DriftTarget> = [
            ("clean.bin", "clean.bin", &[b'n', 1][..]),
            ("upload-match.bin", "upload-match.bin", &[b'u', 0, 0x80][..]),
            ("edited.bin", "edited.bin", &[b'n', 2][..]),
            ("gone.bin", "gone.bin", &[b'n', 3][..]),
            ("brand-new.bin", "brand-new.bin", &[b'n', 4][..]),
        ]
        .into_iter()
        .map(|(remote_path, repo_path, upload)| DriftTarget {
            remote_path: remote_path.to_string(),
            repo_path: repo_path.to_string(),
            upload: crate::drift::UploadFingerprint::of(upload),
        })
        .collect();
        let targets =
            validate_expected_paths(&resolved, targets).expect("the targets should validate");
        clear_commands(&commands);

        let check = check_drift(&mut client, &targets, &resolved)
            .expect("the drift check should complete against the real adapter");

        assert_eq!(check.checked, 5);
        assert!(check.refused);
        assert_eq!(
            check.drifted,
            vec![
                DriftedFile {
                    remote_path: "edited.bin".to_string(),
                    reason: DriftReason::ContentDiffers,
                },
                DriftedFile {
                    remote_path: "gone.bin".to_string(),
                    reason: DriftReason::MissingOnServer,
                },
            ]
        );
        assert_binary_mode_before_first_download(&commands);
        assert_eq!(
            write_commands(&commands),
            Vec::<String>::new(),
            "a drift check must not write"
        );
        assert_eq!(
            client
                .get_bytes("edited.bin")
                .expect("the edited file must remain"),
            vec![b's', 0, 0x80]
        );
        client.quit();
    }

    /// Lends the test's one proxied connection to a deploy run, which would otherwise consume it.
    struct BorrowedClient<'a>(&'a mut FtpClient);

    impl crate::drift::DriftRemote for BorrowedClient<'_> {
        fn set_binary_mode(&mut self) -> Result<(), RemoteFailure> {
            crate::drift::DriftRemote::set_binary_mode(self.0)
        }

        fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure> {
            crate::drift::DriftRemote::download_bytes(self.0, path)
        }
    }

    impl DeployRemote for BorrowedClient<'_> {
        fn mkdir_p(&mut self, path: &str) -> anyhow::Result<()> {
            FtpClient::mkdir_p(self.0, path)
        }

        fn put_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> anyhow::Result<u64> {
            FtpClient::put_bytes(self.0, remote_path, bytes)
        }

        fn put_reader<R: Read>(
            &mut self,
            remote_path: &str,
            reader: &mut R,
        ) -> anyhow::Result<u64> {
            FtpClient::put_reader(self.0, remote_path, reader)
        }

        fn quit(self) {}
    }

    fn clear_commands(commands: &Arc<Mutex<Vec<String>>>) {
        commands
            .lock()
            .expect("command log should not be poisoned")
            .clear();
    }

    fn assert_binary_mode_before_first_download(commands: &Arc<Mutex<Vec<String>>>) {
        let commands = commands.lock().expect("command log should not be poisoned");
        let type_index = commands
            .iter()
            .position(|command| command.starts_with("TYPE I"))
            .expect("the drift check should select binary mode");
        let first_retr_index = commands
            .iter()
            .position(|command| command.starts_with("RETR "))
            .expect("the drift check should download server copies");
        assert!(type_index < first_retr_index);
    }

    fn write_commands(commands: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        commands
            .lock()
            .expect("command log should not be poisoned")
            .iter()
            .filter(|command| {
                [
                    "STOR ", "STOU", "APPE ", "MKD ", "DELE ", "RMD ", "RNFR ", "RNTO ",
                ]
                .iter()
                .any(|write| command.starts_with(write))
            })
            .cloned()
            .collect()
    }

    #[test]
    #[ignore]
    fn disposable_drift_deploy_refuses_a_drifted_server_then_uploads_once_it_matches() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let names = ["a.bin", "b.bin", "c.bin"];
        let base_bytes = |name: &str| [name.as_bytes(), &[0, b'\r', b'\n', 0xff, b'b']].concat();
        let head_bytes = |name: &str| [name.as_bytes(), &[0, b'\r', b'\n', 0x80, b'h']].concat();
        let repo = TestRepo::new();
        for name in names {
            repo.write(name, &base_bytes(name));
        }
        let base = repo.commit_all("base");
        for name in names {
            repo.write(name, &head_bytes(name));
        }
        let profile = crate::config::Profile {
            host: "unused.invalid".to_string(),
            port: 21,
            user: "unused".to_string(),
            // The disposable server's FTP user lives in `/home/test`.
            remote_root: "/home/test".to_string(),
            local_root: repo.path().display().to_string(),
            passive: true,
            tls: false,
            accept_invalid_certs: false,
            ignore: vec![".git".to_string()],
        };
        let (_container, mut client, commands) = disposable_branch_client("drift-deploy");
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        // The pinned image fails MKD on an existing directory, so the files sit in the home directory.
        for name in names {
            let bytes = if name == "c.bin" {
                b"server-only edit".to_vec()
            } else {
                base_bytes(name)
            };
            client
                .upload_bytes(name, &bytes)
                .expect("adapter should seed the server copy");
        }
        clear_commands(&commands);

        let dry = deploy("disposable", &profile, true, Some(&base), || {
            Ok(BorrowedClient(&mut client))
        })
        .expect("the drift dry run should complete");
        let dry_check = dry
            .drift_check
            .expect("a dry run with expect_ref checks drift");
        assert!(dry_check.refused);
        assert_eq!(dry_check.drifted.len(), 1);
        assert_eq!(dry_check.drifted[0].remote_path, "/home/test/c.bin");
        assert_eq!(
            dry.uploaded.len(),
            3,
            "the dry run still lists the planned files"
        );
        assert_eq!(write_commands(&commands), Vec::<String>::new());

        let refused = deploy("disposable", &profile, false, Some(&base), || {
            Ok(BorrowedClient(&mut client))
        })
        .expect("a refusal is a response");
        assert!(refused.drift_check.expect("check present").refused);
        assert_eq!(refused.files_uploaded, 0);
        assert_eq!(refused.bytes_uploaded, 0);
        assert_eq!(refused.directories_created, 0);
        assert_eq!(write_commands(&commands), Vec::<String>::new());
        assert_eq!(
            client.get_bytes("a.bin").expect("a.bin should remain"),
            base_bytes("a.bin"),
            "a clean file stays at its old version when another file drifted"
        );
        assert_eq!(
            client.get_bytes("c.bin").expect("c.bin should remain"),
            b"server-only edit".to_vec()
        );

        client
            .upload_bytes("c.bin", &base_bytes("c.bin"))
            .expect("adapter should restore the base copy");
        clear_commands(&commands);
        let uploaded = deploy("disposable", &profile, false, Some(&base), || {
            Ok(BorrowedClient(&mut client))
        })
        .expect("the clean run should complete");
        assert!(!uploaded.drift_check.expect("check present").refused);
        assert_eq!(uploaded.files_uploaded, 3);
        for name in names {
            assert_eq!(
                client.get_bytes(name).expect("uploaded file should exist"),
                head_bytes(name),
                "{name}"
            );
        }
        assert_binary_mode_before_first_download(&commands);
        let commands = commands.lock().expect("command log should not be poisoned");
        let first_stor_index = commands
            .iter()
            .position(|command| command.starts_with("STOR "))
            .expect("the run should upload");
        let last_check_retr = commands[..first_stor_index]
            .iter()
            .rposition(|command| command.starts_with("RETR "))
            .expect("downloads precede the first upload");
        assert!(last_check_retr < first_stor_index);
        drop(commands);
        client.quit();
    }

    #[test]
    #[ignore]
    fn disposable_drift_upload_file_uploads_a_matching_server_and_refuses_an_edited_one() {
        assert_eq!(
            std::env::var("ZED_FTP_RUN_FTP_INTEGRATION").as_deref(),
            Ok("1"),
            "set ZED_FTP_RUN_FTP_INTEGRATION=1 to run the disposable FTP test"
        );

        let committed = [b'h', 0, b'\r', b'\n', 0xff];
        let edited_locally = [b'w', 0, b'\r', b'\n', 0x80];
        let repo = TestRepo::new();
        repo.write("Mails.php", &committed);
        repo.commit_all("base");
        repo.write("Mails.php", &edited_locally);
        let local = repo.path().join("Mails.php").display().to_string();
        // The disposable server's FTP user lives in `/home/test`.
        let remote = "/home/test/Mails.php";
        let (_container, mut client, commands) = disposable_branch_client("drift-upload");
        client
            .set_binary_mode()
            .expect("adapter should select binary mode");
        client
            .upload_bytes("Mails.php", &committed)
            .expect("adapter should seed the committed copy");
        clear_commands(&commands);

        let uploaded = upload_file(
            &UploadFileRequest {
                local_path: &local,
                remote_path: remote,
                before_changes: false,
                expect_ref: Some("HEAD"),
            },
            || Ok(BorrowedClient(&mut client)),
        )
        .expect("a server copy equal to HEAD is clean");
        assert!(!uploaded.drift_check.expect("check present").refused);
        assert_eq!(uploaded.bytes, edited_locally.len() as u64);
        assert_eq!(
            client.get_bytes("Mails.php").expect("file should exist"),
            edited_locally.to_vec()
        );

        client
            .upload_bytes("Mails.php", b"server-only edit")
            .expect("adapter should seed the server-only edit");
        clear_commands(&commands);
        let refused = upload_file(
            &UploadFileRequest {
                local_path: &local,
                remote_path: remote,
                before_changes: false,
                expect_ref: Some("HEAD"),
            },
            || Ok(BorrowedClient(&mut client)),
        )
        .expect("a refusal is a response");
        let check = refused.drift_check.expect("check present");
        assert!(check.refused);
        assert_eq!(check.drifted[0].remote_path, remote);
        assert_eq!(refused.bytes, 0);
        assert_eq!(write_commands(&commands), Vec::<String>::new());
        assert_eq!(
            client.get_bytes("Mails.php").expect("file should remain"),
            b"server-only edit".to_vec()
        );
        client.quit();
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

    /// Starts a disposable FTP container and returns a client that talks to it through a
    /// control-command logging proxy.
    fn disposable_branch_client(
        name_suffix: &str,
    ) -> (DockerContainer, FtpClient, Arc<Mutex<Vec<String>>>) {
        let passive_port = reserve_port();
        let container_name = format!(
            "zed-ftp-branch-{name_suffix}-test-{}",
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
        let container = DockerContainer {
            name: container_name,
        };
        let control_port = published_control_port(&container.name);
        wait_for_ftp(control_port);
        let (proxy_port, commands) = start_control_proxy(control_port);

        let mut stream = FtpStream::connect(format!("127.0.0.1:{proxy_port}"))
            .expect("FTP client should connect through the control proxy");
        stream.set_mode(Mode::Passive);
        stream
            .login("test", "test")
            .expect("FTP client should authenticate");
        let client = FtpClient {
            stream: AnyFtpStream::Plain(stream),
        };
        (container, client, commands)
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
