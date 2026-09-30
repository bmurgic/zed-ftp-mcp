//! Drift guard for the upload tools.
//!
//! With `expect_ref` set, `ftp_upload_file`, `ftp_deploy`, and `ftp_deploy_commits` download each
//! target file's server copy before uploading anything. A file is drifted when the server holds
//! content that is neither the copy at `expect_ref` nor the copy the tool is about to upload. Any
//! drifted file refuses the whole run.

// Temporary: the deploy tools that call this module arrive in the next commit.
#![allow(dead_code)]

use crate::branch_deploy::RemoteFailure;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The server side of a drift check. `FtpClient` implements it through `download_or_missing`.
pub trait DriftRemote {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure>;
    /// Downloads a server file. `Ok(None)` means the file is missing, which only an FTP 550
    /// reply confirmed by the parent listing may report. Every other error is a failure.
    fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure>;
}

/// `expect_ref` resolved to a commit inside the repository that holds the target files.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    repo_root: PathBuf,
    expect_ref: String,
    commit: String,
}

impl ResolvedRef {
    /// The canonical root of the Git worktree the ref was resolved in.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

/// One file the tool would upload.
#[derive(Debug, Clone)]
pub struct DriftTarget {
    /// The full server path, including the profile's remote root.
    pub remote_path: String,
    /// The file's path relative to the repository root, with `/` separators.
    pub repo_path: String,
    /// The exact bytes the tool would upload.
    pub upload_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriftReason {
    /// The server copy exists and equals neither the expected copy nor the upload copy.
    ContentDiffers,
    /// The server copy is missing, but the file exists at `expect_ref`.
    MissingOnServer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct DriftedFile {
    /// The full server path, including the profile's remote root.
    pub remote_path: String,
    pub reason: DriftReason,
}

/// The drift-check result. It appears in a response only when `expect_ref` was supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct DriftCheck {
    /// The ref as requested.
    pub expect_ref: String,
    /// The full commit identifier the ref resolved to.
    pub resolved_commit: String,
    /// The number of target files checked.
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub checked: usize,
    /// True when at least one file drifted. A refused run uploads nothing.
    pub refused: bool,
    pub drifted: Vec<DriftedFile>,
}

impl DriftCheck {
    /// The result for a run that has no target files, so nothing was downloaded.
    pub fn without_targets(resolved: &ResolvedRef) -> Self {
        Self {
            expect_ref: resolved.expect_ref.clone(),
            resolved_commit: resolved.commit.clone(),
            checked: 0,
            refused: false,
            drifted: Vec::new(),
        }
    }
}

/// `InvalidArgs` is a mistake the caller can fix. It is always raised before an FTP connection
/// opens, except when `check_drift` itself finds a target whose path at `expect_ref` is not a file.
#[derive(thiserror::Error, Debug)]
pub enum DriftError {
    #[error("{0}")]
    InvalidArgs(String),
    #[error("downloading {remote_path} for the drift check failed: {error}")]
    Download { remote_path: String, error: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Resolves `expect_ref` to a commit in the Git worktree that contains `repo_dir`.
/// Tags are peeled, and the object must exist.
pub fn resolve_expect_ref(repo_dir: &Path, expect_ref: &str) -> Result<ResolvedRef, DriftError> {
    // A leading dash would make git read the ref as an option.
    if expect_ref.is_empty() || expect_ref.starts_with('-') {
        return Err(DriftError::InvalidArgs(format!(
            "expect_ref '{expect_ref}' is not a Git ref"
        )));
    }

    let top_level = run_git(repo_dir, &["rev-parse", "--show-toplevel"])?;
    if !top_level.status.success() {
        return Err(DriftError::InvalidArgs(format!(
            "expect_ref needs a Git worktree, and '{}' is not inside one: {}",
            repo_dir.display(),
            stderr_text(&top_level)
        )));
    }
    let repo_root = PathBuf::from(stdout_text(&top_level))
        .canonicalize()
        .map_err(|error| {
            DriftError::InvalidArgs(format!("Git worktree root cannot be resolved: {error}"))
        })?;

    let peeled = format!("{expect_ref}^{{commit}}");
    let commit_output = run_git(&repo_root, &["rev-parse", "--verify", peeled.as_str()])?;
    if !commit_output.status.success() {
        return Err(DriftError::InvalidArgs(format!(
            "expect_ref '{expect_ref}' does not resolve to a commit: {}",
            stderr_text(&commit_output)
        )));
    }
    let commit = stdout_text(&commit_output);
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DriftError::Other(anyhow::anyhow!(
            "git returned an invalid commit identifier '{commit}'"
        )));
    }

    Ok(ResolvedRef {
        repo_root,
        expect_ref: expect_ref.to_string(),
        commit,
    })
}

/// A file's path relative to the repository root, with `/` separators.
pub fn repo_relative_path(resolved: &ResolvedRef, file: &Path) -> Result<String, DriftError> {
    let absolute = file.canonicalize().map_err(|error| {
        DriftError::InvalidArgs(format!("cannot resolve {}: {error}", file.display()))
    })?;
    let relative = absolute.strip_prefix(&resolved.repo_root).map_err(|_| {
        DriftError::InvalidArgs(format!(
            "{} is not inside the Git worktree {}",
            file.display(),
            resolved.repo_root.display()
        ))
    })?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

/// Rejects any target whose path at `expect_ref` is a directory, a submodule, or a symbolic link.
/// Callers run this before they open an FTP connection. `check_drift` repeats the same rule.
pub fn validate_expected_paths(
    resolved: &ResolvedRef,
    targets: &[DriftTarget],
) -> Result<(), DriftError> {
    for target in targets {
        expected_blob_id(resolved, &target.repo_path)?;
    }
    Ok(())
}

/// Downloads every target's server copy and classifies it against the copy at `expect_ref` and
/// the upload copy. It stops at the first download failure and never classifies past it.
pub fn check_drift<R: DriftRemote>(
    remote: &mut R,
    targets: &[DriftTarget],
    resolved: &ResolvedRef,
) -> Result<DriftCheck, DriftError> {
    remote.set_binary_mode().map_err(|failure| {
        DriftError::Other(anyhow::anyhow!(
            "selecting binary mode for the drift check failed: {}",
            failure.error
        ))
    })?;

    let mut drifted = Vec::new();
    for target in targets {
        let expected = expected_copy(resolved, &target.repo_path)?;
        let server = remote
            .download_bytes(&target.remote_path)
            .map_err(|failure| DriftError::Download {
                remote_path: target.remote_path.clone(),
                error: failure.error,
            })?;
        if let Some(reason) = classify(server.as_deref(), expected.as_deref(), &target.upload_bytes)
        {
            drifted.push(DriftedFile {
                remote_path: target.remote_path.clone(),
                reason,
            });
        }
    }

    Ok(DriftCheck {
        expect_ref: resolved.expect_ref.clone(),
        resolved_commit: resolved.commit.clone(),
        checked: targets.len(),
        refused: !drifted.is_empty(),
        drifted,
    })
}

/// `None` means the file is clean.
fn classify(server: Option<&[u8]>, expected: Option<&[u8]>, upload: &[u8]) -> Option<DriftReason> {
    match (server, expected) {
        (Some(server), Some(expected)) => {
            (server != expected && server != upload).then_some(DriftReason::ContentDiffers)
        }
        (Some(server), None) => (server != upload).then_some(DriftReason::ContentDiffers),
        (None, Some(_)) => Some(DriftReason::MissingOnServer),
        (None, None) => None,
    }
}

/// The file's content at `expect_ref`, or `None` when the path does not exist there.
fn expected_copy(resolved: &ResolvedRef, repo_path: &str) -> Result<Option<Vec<u8>>, DriftError> {
    let Some(blob_id) = expected_blob_id(resolved, repo_path)? else {
        return Ok(None);
    };
    let output = run_git(&resolved.repo_root, &["cat-file", "blob", blob_id.as_str()])?;
    if !output.status.success() {
        return Err(DriftError::Other(anyhow::anyhow!(
            "git cat-file failed for {repo_path} at {}: {}",
            resolved.commit,
            stderr_text(&output)
        )));
    }
    Ok(Some(output.stdout))
}

/// The blob ID of the regular file at `repo_path` in the resolved commit, `None` when the path
/// has no entry there, and `InvalidArgs` when the entry is anything but a regular file.
fn expected_blob_id(resolved: &ResolvedRef, repo_path: &str) -> Result<Option<String>, DriftError> {
    let output = run_git(
        &resolved.repo_root,
        &["ls-tree", "-z", resolved.commit.as_str(), "--", repo_path],
    )?;
    if !output.status.success() {
        return Err(DriftError::Other(anyhow::anyhow!(
            "git ls-tree failed for {repo_path} at {}: {}",
            resolved.commit,
            stderr_text(&output)
        )));
    }

    // `ls-tree` matches whole path components, so a path prints at most its own entry.
    let Some(record) = output
        .stdout
        .split(|byte| *byte == b'\0')
        .next()
        .filter(|record| !record.is_empty())
    else {
        return Ok(None);
    };
    let metadata = record
        .split(|byte| *byte == b'\t')
        .next()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let fields: Vec<&str> = metadata.split(' ').collect();
    let [mode, object_type, object_id] = fields.as_slice() else {
        return Err(DriftError::Other(anyhow::anyhow!(
            "malformed git ls-tree record for {repo_path}"
        )));
    };
    let is_regular_file = *object_type == "blob" && matches!(*mode, "100644" | "100755");
    if !is_regular_file {
        return Err(DriftError::InvalidArgs(format!(
            "{repo_path} is not a regular file at expect_ref '{}' (it is a {object_type} with mode {mode})",
            resolved.expect_ref
        )));
    }
    Ok(Some((*object_id).to_string()))
}

fn run_git(directory: &Path, arguments: &[&str]) -> Result<Output, DriftError> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                DriftError::InvalidArgs(
                    "`git` was not found on PATH; install git or add it to your PATH".to_string(),
                )
            } else {
                DriftError::Other(anyhow::Error::from(error).context("spawning git"))
            }
        })
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// A scripted server. A path with a scripted failure fails, a path in `files` downloads, and
    /// any other path is missing.
    #[derive(Default)]
    pub(crate) struct FakeRemote {
        pub(crate) files: BTreeMap<String, Vec<u8>>,
        pub(crate) failures: BTreeMap<String, RemoteFailure>,
        pub(crate) binary_mode_failure: Option<RemoteFailure>,
        pub(crate) calls: Vec<String>,
    }

    impl DriftRemote for FakeRemote {
        fn set_binary_mode(&mut self) -> Result<(), RemoteFailure> {
            self.calls.push("TYPE I".to_string());
            match &self.binary_mode_failure {
                Some(failure) => Err(failure.clone()),
                None => Ok(()),
            }
        }

        fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure> {
            self.calls.push(format!("RETR {path}"));
            if let Some(failure) = self.failures.get(path) {
                return Err(failure.clone());
            }
            Ok(self.files.get(path).cloned())
        }
    }

    /// A temporary Git repository with a fixed identity.
    pub(crate) struct TestRepo {
        directory: TempDir,
    }

    impl TestRepo {
        pub(crate) fn new() -> Self {
            let repo = Self {
                directory: TempDir::new().expect("temp directory should be created"),
            };
            repo.git_success(&["init", "-q"]);
            repo
        }

        pub(crate) fn path(&self) -> &Path {
            self.directory.path()
        }

        pub(crate) fn write(&self, relative_path: &str, bytes: &[u8]) {
            let path = self.path().join(relative_path);
            std::fs::create_dir_all(path.parent().expect("fixture path has a parent"))
                .expect("fixture directory should be created");
            std::fs::write(path, bytes).expect("fixture file should be written");
        }

        pub(crate) fn commit_all(&self, message: &str) -> String {
            self.git_success(&["add", "-A"]);
            self.git_success(&["commit", "-q", "-m", message]);
            self.git_text(&["rev-parse", "HEAD"])
        }

        pub(crate) fn git_text(&self, arguments: &[&str]) -> String {
            let output = self.git_success(arguments);
            String::from_utf8(output.stdout)
                .expect("git output should be UTF-8")
                .trim()
                .to_string()
        }

        pub(crate) fn git_success(&self, arguments: &[&str]) -> Output {
            let output = Command::new("git")
                .arg("-C")
                .arg(self.path())
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(arguments)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Drift Test")
                .env("GIT_AUTHOR_EMAIL", "drift@example.test")
                .env("GIT_COMMITTER_NAME", "Drift Test")
                .env("GIT_COMMITTER_EMAIL", "drift@example.test")
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {arguments:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }
    }

    fn target(remote_path: &str, repo_path: &str, upload: &[u8]) -> DriftTarget {
        DriftTarget {
            remote_path: remote_path.to_string(),
            repo_path: repo_path.to_string(),
            upload_bytes: upload.to_vec(),
        }
    }

    fn expect_invalid_args<T: std::fmt::Debug>(result: Result<T, DriftError>) -> String {
        match result {
            Err(DriftError::InvalidArgs(message)) => message,
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn expected_ref_02_unresolvable_expected_ref_is_rejected() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"one\n");
        repo.commit_all("base");

        for bad_ref in [
            "no-such-branch",
            "0000000000000000000000000000000000000000",
            "HEAD:a.txt",
            "",
            "--all",
        ] {
            let message = expect_invalid_args(resolve_expect_ref(repo.path(), bad_ref));
            assert!(!message.is_empty(), "{bad_ref}");
        }
    }

    #[test]
    fn expect_ref_peels_a_tag_and_a_branch_to_the_full_commit() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"one\n");
        let commit = repo.commit_all("base");
        repo.git_success(&["tag", "-a", "-m", "release", "release"]);
        repo.git_success(&["branch", "deployed"]);

        for reference in ["release", "deployed", "HEAD"] {
            let resolved = resolve_expect_ref(repo.path(), reference).expect("ref should resolve");
            assert_eq!(resolved.commit, commit, "{reference}");
            assert_eq!(resolved.expect_ref, reference);
        }
    }

    #[test]
    fn expected_ref_03_upload_source_outside_a_repository() {
        let outside = TempDir::new().expect("temp directory should be created");

        let message = expect_invalid_args(resolve_expect_ref(outside.path(), "HEAD"));

        assert!(message.contains("Git worktree"), "{message}");
    }

    #[test]
    fn expect_ref_resolves_from_a_subdirectory_to_the_repository_root() {
        let repo = TestRepo::new();
        repo.write("site/a.txt", b"one\n");
        repo.commit_all("base");

        let resolved = resolve_expect_ref(&repo.path().join("site"), "HEAD")
            .expect("a subdirectory of the worktree is inside the repository");

        assert_eq!(
            resolved.repo_root(),
            repo.path().canonicalize().expect("repo path exists")
        );
        assert_eq!(
            repo_relative_path(&resolved, &repo.path().join("site/a.txt"))
                .expect("file is inside the repository"),
            "site/a.txt"
        );
    }

    #[test]
    fn repo_relative_path_rejects_a_file_outside_the_repository() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"one\n");
        repo.commit_all("base");
        let outside = TempDir::new().expect("temp directory should be created");
        std::fs::write(outside.path().join("loose.txt"), b"x").expect("fixture should be written");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");

        expect_invalid_args(repo_relative_path(
            &resolved,
            &outside.path().join("loose.txt"),
        ));
        expect_invalid_args(repo_relative_path(
            &resolved,
            &repo.path().join("missing.txt"),
        ));
    }

    #[test]
    fn expected_ref_04_expected_path_is_not_a_regular_file() {
        let repo = TestRepo::new();
        repo.write("folder/inner.txt", b"inner\n");
        repo.write("target.txt", b"target\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink("target.txt", repo.path().join("link.txt"))
            .expect("symlink should be created");
        repo.git_success(&["add", "-A"]);
        let gitlink = format!("160000,{},module", "1".repeat(40));
        repo.git_success(&["update-index", "--add", "--cacheinfo", gitlink.as_str()]);
        repo.git_success(&["commit", "-q", "-m", "base"]);
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");

        let mut not_regular = vec!["folder", "module"];
        if cfg!(unix) {
            not_regular.push("link.txt");
        }
        for repo_path in not_regular {
            let targets = [target("/site/x", repo_path, b"upload")];

            let message = expect_invalid_args(validate_expected_paths(&resolved, &targets));
            assert!(message.contains(repo_path), "{message}");
            let mut remote = FakeRemote::default();
            expect_invalid_args(check_drift(&mut remote, &targets, &resolved));
        }
        validate_expected_paths(&resolved, &[target("/site/x", "target.txt", b"upload")])
            .expect("a regular file is accepted");
    }

    #[test]
    fn expected_copy_matches_the_exact_path_only() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"only a.txt\n");
        repo.write("dir/a", b"nested a\n");
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");

        assert_eq!(
            expected_copy(&resolved, "a").expect("a is absent, not invalid"),
            None
        );
        assert_eq!(
            expected_copy(&resolved, "dir/a").expect("nested file is a regular file"),
            Some(b"nested a\n".to_vec())
        );
        assert_eq!(
            expected_copy(&resolved, "a.txt").expect("top-level file is a regular file"),
            Some(b"only a.txt\n".to_vec())
        );
    }

    #[test]
    fn drift_classification_01_classification_table() {
        let repo = TestRepo::new();
        repo.write("site/present.txt", b"expected\n");
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");
        let upload: &[u8] = b"upload\n";
        struct Row {
            name: &'static str,
            server: Option<&'static [u8]>,
            repo_path: &'static str,
            reason: Option<DriftReason>,
        }
        let rows = [
            Row {
                name: "equal to the expected copy, present",
                server: Some(b"expected\n"),
                repo_path: "site/present.txt",
                reason: None,
            },
            Row {
                name: "equal to the upload copy, present",
                server: Some(b"upload\n"),
                repo_path: "site/present.txt",
                reason: None,
            },
            Row {
                name: "equal to the upload copy, absent",
                server: Some(b"upload\n"),
                repo_path: "site/absent.txt",
                reason: None,
            },
            Row {
                name: "missing, absent",
                server: None,
                repo_path: "site/absent.txt",
                reason: None,
            },
            Row {
                name: "different from both copies, present",
                server: Some(b"server\n"),
                repo_path: "site/present.txt",
                reason: Some(DriftReason::ContentDiffers),
            },
            Row {
                name: "different from the upload copy, absent",
                server: Some(b"server\n"),
                repo_path: "site/absent.txt",
                reason: Some(DriftReason::ContentDiffers),
            },
            Row {
                name: "missing, present",
                server: None,
                repo_path: "site/present.txt",
                reason: Some(DriftReason::MissingOnServer),
            },
        ];

        for Row {
            name,
            server,
            repo_path,
            reason: expected_reason,
        } in rows
        {
            let mut remote = FakeRemote::default();
            if let Some(bytes) = server {
                remote
                    .files
                    .insert("/site/file".to_string(), bytes.to_vec());
            }

            let check = check_drift(
                &mut remote,
                &[target("/site/file", repo_path, upload)],
                &resolved,
            )
            .expect("the drift check should complete");

            let reasons: Vec<DriftReason> = check.drifted.iter().map(|d| d.reason).collect();
            assert_eq!(
                reasons,
                expected_reason.into_iter().collect::<Vec<_>>(),
                "{name}"
            );
            assert_eq!(check.refused, expected_reason.is_some(), "{name}");
            assert_eq!(check.checked, 1, "{name}");
        }
    }

    #[test]
    fn drift_check_lists_every_drifted_file_in_target_order_with_its_full_server_path() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"a\n");
        repo.write("b.txt", b"b\n");
        repo.write("c.txt", b"c\n");
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");
        let mut remote = FakeRemote::default();
        remote
            .files
            .insert("/root/a.txt".to_string(), b"a\n".to_vec());
        remote
            .files
            .insert("/root/c.txt".to_string(), b"edited c\n".to_vec());
        let targets = [
            target("/root/c.txt", "c.txt", b"new c\n"),
            target("/root/a.txt", "a.txt", b"new a\n"),
            target("/root/b.txt", "b.txt", b"new b\n"),
        ];

        let check =
            check_drift(&mut remote, &targets, &resolved).expect("the drift check should complete");

        assert_eq!(check.checked, 3);
        assert!(check.refused);
        assert_eq!(
            check.drifted,
            vec![
                DriftedFile {
                    remote_path: "/root/c.txt".to_string(),
                    reason: DriftReason::ContentDiffers,
                },
                DriftedFile {
                    remote_path: "/root/b.txt".to_string(),
                    reason: DriftReason::MissingOnServer,
                },
            ]
        );
    }

    #[test]
    fn drift_check_selects_binary_mode_before_the_first_download() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"a\n");
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");
        let mut remote = FakeRemote::default();

        check_drift(
            &mut remote,
            &[target("/r/a.txt", "a.txt", b"a\n")],
            &resolved,
        )
        .expect("the drift check should complete");

        assert_eq!(remote.calls, vec!["TYPE I", "RETR /r/a.txt"]);
    }

    #[test]
    fn drift_check_downloads_nothing_when_binary_mode_fails() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"a\n");
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");
        let mut remote = FakeRemote {
            binary_mode_failure: Some(RemoteFailure::operation("500 no TYPE")),
            ..FakeRemote::default()
        };

        let error = check_drift(
            &mut remote,
            &[target("/r/a.txt", "a.txt", b"a\n")],
            &resolved,
        )
        .expect_err("a failed binary mode must fail the check");

        assert!(matches!(error, DriftError::Other(_)), "{error:?}");
        assert_eq!(remote.calls, vec!["TYPE I"]);
    }

    #[test]
    fn drift_classification_03_download_error_is_not_treated_as_missing() {
        let repo = TestRepo::new();
        for name in ["one", "two", "three"] {
            repo.write(&format!("{name}.txt"), b"committed\n");
        }
        repo.commit_all("base");
        let resolved = resolve_expect_ref(repo.path(), "HEAD").expect("ref should resolve");
        let failures = [
            (
                "a 451 local error",
                RemoteFailure::operation("451 Local error"),
            ),
            (
                "a 550 reply while the parent listing contains the file name",
                RemoteFailure::operation("550 Failed to open file"),
            ),
            (
                "a lost connection",
                RemoteFailure::connection_lost("connection reset"),
            ),
        ];

        for (name, failure) in failures {
            let mut remote = FakeRemote::default();
            remote
                .files
                .insert("/r/one.txt".to_string(), b"server edit\n".to_vec());
            remote
                .failures
                .insert("/r/two.txt".to_string(), failure.clone());
            let targets = [
                target("/r/one.txt", "one.txt", b"upload\n"),
                target("/r/two.txt", "two.txt", b"upload\n"),
                target("/r/three.txt", "three.txt", b"upload\n"),
            ];

            let error = check_drift(&mut remote, &targets, &resolved)
                .expect_err("a download failure must fail the whole check");

            match error {
                DriftError::Download { remote_path, error } => {
                    assert_eq!(remote_path, "/r/two.txt", "{name}");
                    assert_eq!(error, failure.error, "{name}");
                }
                other => panic!("{name}: expected a download error, got {other:?}"),
            }
            assert_eq!(
                remote.calls,
                vec!["TYPE I", "RETR /r/one.txt", "RETR /r/two.txt"],
                "{name}: nothing is downloaded after the failure"
            );
        }
    }

    #[test]
    fn drift_check_result_serializes_to_the_documented_shape() {
        let refused = DriftCheck {
            expect_ref: "base".to_string(),
            resolved_commit: "a".repeat(40),
            checked: 2,
            refused: true,
            drifted: vec![
                DriftedFile {
                    remote_path: "/home/test/c.txt".to_string(),
                    reason: DriftReason::ContentDiffers,
                },
                DriftedFile {
                    remote_path: "/home/test/d.txt".to_string(),
                    reason: DriftReason::MissingOnServer,
                },
            ],
        };
        let clean = DriftCheck {
            refused: false,
            drifted: Vec::new(),
            ..refused.clone()
        };

        assert_eq!(
            serde_json::to_value(&refused).expect("result should serialize"),
            serde_json::json!({
                "expect_ref": "base",
                "resolved_commit": "a".repeat(40),
                "checked": 2,
                "refused": true,
                "drifted": [
                    {"remote_path": "/home/test/c.txt", "reason": "content_differs"},
                    {"remote_path": "/home/test/d.txt", "reason": "missing_on_server"}
                ]
            })
        );
        assert_eq!(
            serde_json::to_value(&clean).expect("result should serialize")["drifted"],
            serde_json::json!([])
        );
    }
}
