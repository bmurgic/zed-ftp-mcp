//! Recursive local → remote upload, gitignore-aware.
//!
//! Strategy:
//!   1. Build a file list (full walk, or `git diff-tree` for commit-scoped).
//!   2. For each file, compute its remote path: `remote_root / rel`.
//!   3. Pre-create the unique set of parent directories on the server.
//!   4. Upload each file via `STOR`.
//!
//! Returns a structured summary so the agent can show what happened (or
//! what would have happened, in `dry_run` mode).

use crate::config::Profile;
use crate::drift::{self, DriftCheck, DriftError, DriftRemote, DriftTarget, ResolvedRef};
use crate::ftp::FtpClient;
use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeployPlan {
    pub profile: String,
    pub local_root: String,
    pub remote_root: String,
    pub dry_run: bool,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub files_uploaded: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes_uploaded: u64,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub directories_created: usize,
    pub skipped: Vec<String>,
    pub uploaded: Vec<UploadedFile>,
    /// Present only when `expect_ref` was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_check: Option<DriftCheck>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UploadedFile {
    pub local: String,
    pub remote: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: u64,
}

/// Routable error for the deploy functions so the tool layer can return
/// `invalid_params` for user-fixable mistakes (bad SHA, bad `expect_ref`, missing git) and
/// `internal_error` for everything else.
#[derive(thiserror::Error, Debug)]
pub enum DeployError {
    #[error("{0}")]
    InvalidArgs(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<DriftError> for DeployError {
    fn from(error: DriftError) -> Self {
        match error {
            DriftError::InvalidArgs(message) => DeployError::InvalidArgs(message),
            DriftError::Download { .. } => DeployError::Other(anyhow::anyhow!("{error}")),
            DriftError::Other(error) => DeployError::Other(error),
        }
    }
}

/// The server operations a deploy run needs, on top of the drift check's downloads. `FtpClient`
/// implements it, and tests use an in-memory fake.
pub(crate) trait DeployRemote: DriftRemote {
    fn mkdir_p(&mut self, path: &str) -> Result<()>;
    /// Uploads `bytes`, creating the parent directories first.
    fn put_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> Result<u64>;
    fn put_reader<R: Read>(&mut self, remote_path: &str, reader: &mut R) -> Result<u64>;
    fn quit(self);
}

/// Full-tree deploy: walk `local_root` honoring .gitignore and per-profile
/// ignore patterns, then upload everything that survives the filter.
/// With `expect_ref`, the run first checks every target for server-side drift.
pub fn deploy(
    profile_name: &str,
    profile: &Profile,
    dry_run: bool,
    expect_ref: Option<&str>,
) -> std::result::Result<DeployPlan, DeployError> {
    deploy_with(profile_name, profile, dry_run, expect_ref, || {
        FtpClient::connect(profile_name, profile)
    })
}

pub(crate) fn deploy_with<R: DeployRemote>(
    profile_name: &str,
    profile: &Profile,
    dry_run: bool,
    expect_ref: Option<&str>,
    connect: impl FnOnce() -> Result<R>,
) -> std::result::Result<DeployPlan, DeployError> {
    let local_root = canon_local_root(profile)?;
    let files = walk_files(&local_root, &profile.ignore)?;
    upload_files(
        connect,
        profile_name,
        profile,
        &local_root,
        files,
        dry_run,
        expect_ref,
    )
}

/// Commit-scoped deploy: union the file lists from each commit's
/// `git diff-tree` and upload only those files (current working-tree state).
/// With `expect_ref`, the run first checks every target for server-side drift.
pub fn deploy_commits(
    profile_name: &str,
    profile: &Profile,
    commits: &[String],
    dry_run: bool,
    expect_ref: Option<&str>,
) -> std::result::Result<DeployPlan, DeployError> {
    deploy_commits_with(profile_name, profile, commits, dry_run, expect_ref, || {
        FtpClient::connect(profile_name, profile)
    })
}

fn deploy_commits_with<R: DeployRemote>(
    profile_name: &str,
    profile: &Profile,
    commits: &[String],
    dry_run: bool,
    expect_ref: Option<&str>,
    connect: impl FnOnce() -> Result<R>,
) -> std::result::Result<DeployPlan, DeployError> {
    if commits.is_empty() {
        return Err(DeployError::InvalidArgs(
            "commits list is empty".to_string(),
        ));
    }

    let local_root = canon_local_root(profile).map_err(DeployError::Other)?;

    // Union of all changed paths across the requested commits.
    let mut rel_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for sha in commits {
        for p in changed_paths_for_commit(&local_root, sha)? {
            rel_paths.insert(p);
        }
    }

    // Apply the same profile ignore patterns used by the full-tree walk.
    let mut overrides = OverrideBuilder::new(&local_root);
    for pat in &profile.ignore {
        overrides
            .add(&format!("!{pat}"))
            .with_context(|| format!("invalid ignore pattern '{pat}'"))
            .map_err(DeployError::Other)?;
    }
    let overrides = overrides
        .build()
        .context("building ignore overrides")
        .map_err(DeployError::Other)?;

    // Belt-and-suspenders: even with --diff-filter=ACMRT, rename sources or
    // race-deleted files might not exist on disk. Skip those silently.
    let files: Vec<PathBuf> = rel_paths
        .into_iter()
        .filter(|rel| overrides.matched(rel, false).is_none())
        .map(|rel| local_root.join(rel))
        .filter(|p| p.is_file())
        .collect();

    upload_files(
        connect,
        profile_name,
        profile,
        &local_root,
        files,
        dry_run,
        expect_ref,
    )
}

/// One `ftp_upload_file` call.
pub struct UploadFileRequest<'a> {
    pub local_path: &'a str,
    /// The full server path, including the profile's remote root.
    pub remote_path: &'a str,
    /// Upload the last-committed (git HEAD) version instead of the working-tree content.
    pub before_changes: bool,
    pub expect_ref: Option<&'a str>,
}

#[derive(Debug)]
pub struct UploadFileOutcome {
    /// The bytes uploaded. A refused upload writes nothing, so this is 0.
    pub bytes: u64,
    /// Present only when `expect_ref` was supplied.
    pub drift_check: Option<DriftCheck>,
}

/// Uploads one file. With `expect_ref`, the file's server copy is checked for drift first, and a
/// drifted file is left unchanged.
pub fn upload_file(
    profile_name: &str,
    profile: &Profile,
    request: &UploadFileRequest,
) -> std::result::Result<UploadFileOutcome, DeployError> {
    upload_file_with(request, || FtpClient::connect(profile_name, profile))
}

pub(crate) fn upload_file_with<R: DeployRemote>(
    request: &UploadFileRequest,
    connect: impl FnOnce() -> Result<R>,
) -> std::result::Result<UploadFileOutcome, DeployError> {
    let Some(expect_ref) = request.expect_ref else {
        let content = if request.before_changes {
            committed_content(request.local_path)?
        } else {
            std::fs::read(request.local_path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", request.local_path))?
        };
        let mut client = connect()?;
        let written = client.put_bytes(request.remote_path, &content)?;
        client.quit();
        return Ok(UploadFileOutcome {
            bytes: written,
            drift_check: None,
        });
    };

    let local = Path::new(request.local_path).canonicalize().map_err(|e| {
        DeployError::InvalidArgs(format!("cannot read {}: {e}", request.local_path))
    })?;
    let repo_dir = local
        .parent()
        .ok_or_else(|| DeployError::InvalidArgs(format!("{} has no directory", local.display())))?;
    let resolved = drift::resolve_expect_ref(repo_dir, expect_ref)?;
    let upload_bytes = if request.before_changes {
        committed_content(request.local_path)?
    } else {
        std::fs::read(&local).map_err(|e| {
            DeployError::InvalidArgs(format!("cannot read {}: {e}", request.local_path))
        })?
    };
    let targets = [DriftTarget {
        remote_path: request.remote_path.to_string(),
        repo_path: drift::repo_relative_path(&resolved, &local)?,
        upload_bytes,
    }];
    drift::validate_expected_paths(&resolved, &targets)?;

    let mut client = connect()?;
    let check = drift::check_drift(&mut client, &targets, &resolved)?;
    let written = if check.refused {
        0
    } else {
        client.put_bytes(request.remote_path, &targets[0].upload_bytes)?
    };
    client.quit();
    Ok(UploadFileOutcome {
        bytes: written,
        drift_check: Some(check),
    })
}

/// The file's content at git HEAD, found through the repository that contains it.
fn committed_content(local_path: &str) -> Result<Vec<u8>> {
    let local = Path::new(local_path);
    let parent = local.parent().unwrap_or(Path::new("."));
    let root_out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(parent)
        .output()
        .map_err(|e| anyhow::anyhow!("git rev-parse: {e}"))?;
    if !root_out.status.success() {
        return Err(anyhow::anyhow!(
            "not a git repo: {}",
            String::from_utf8_lossy(&root_out.stderr).trim()
        ));
    }
    let git_root = PathBuf::from(String::from_utf8_lossy(&root_out.stdout).trim());
    let abs = local
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize {local_path}: {e}"))?;
    let rel = abs
        .strip_prefix(&git_root)
        .map_err(|_| anyhow::anyhow!("file not under git root {}", git_root.display()))?;
    let rel_str = rel.to_string_lossy();
    let show_out = Command::new("git")
        .args(["show", &format!("HEAD:{rel_str}")])
        .current_dir(&git_root)
        .output()
        .map_err(|e| anyhow::anyhow!("git show: {e}"))?;
    if !show_out.status.success() {
        return Err(anyhow::anyhow!(
            "git show HEAD:{rel_str} failed: {}",
            String::from_utf8_lossy(&show_out.stderr).trim()
        ));
    }
    Ok(show_out.stdout)
}

// ─── Internals ───────────────────────────────────────────────────────────────

fn canon_local_root(profile: &Profile) -> Result<PathBuf> {
    PathBuf::from(&profile.local_root)
        .canonicalize()
        .with_context(|| format!("local_root '{}' does not exist", profile.local_root))
}

/// Run `git diff-tree --no-commit-id -r --name-only --diff-filter=ACMRT <sha>`
/// inside `local_root`. Returns relative paths.
fn changed_paths_for_commit(
    local_root: &Path,
    sha: &str,
) -> std::result::Result<Vec<PathBuf>, DeployError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(local_root)
        .args([
            "diff-tree",
            "--no-commit-id",
            "-r",
            "--name-only",
            "--diff-filter=ACMRT",
            "--first-parent",
        ])
        .arg(sha)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DeployError::InvalidArgs(
                    "`git` was not found on PATH; install git or add it to your PATH".to_string(),
                )
            } else {
                DeployError::Other(anyhow::Error::from(e).context("spawning git diff-tree"))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("git diff-tree failed for '{sha}' (exit {})", output.status)
        } else {
            format!("git diff-tree failed for '{sha}': {stderr}")
        };
        return Err(DeployError::InvalidArgs(msg));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    Ok(paths)
}

/// Shared upload pipeline: turn a Vec<absolute path under local_root> into a
/// DeployPlan, optionally as a dry run.
///
/// `connect` opens the run's single FTP connection. Without `expect_ref`, a dry run never calls
/// it. With `expect_ref`, every failure that the caller can fix is raised before it is called,
/// and the drift check runs before any directory is created or file uploaded.
pub(crate) fn upload_files<R: DeployRemote>(
    connect: impl FnOnce() -> Result<R>,
    profile_name: &str,
    profile: &Profile,
    local_root: &Path,
    files: Vec<PathBuf>,
    dry_run: bool,
    expect_ref: Option<&str>,
) -> std::result::Result<DeployPlan, DeployError> {
    let remote_root = profile.remote_root.trim_end_matches('/').to_string();
    let mut parents: BTreeSet<String> = BTreeSet::new();
    let mut planned: Vec<(PathBuf, String, u64)> = Vec::with_capacity(files.len());

    for path in &files {
        let rel = path.strip_prefix(local_root).with_context(|| {
            format!(
                "file {} is not under local_root {}",
                path.display(),
                local_root.display()
            )
        })?;
        let rel_str = path_to_posix(rel);
        let remote = if remote_root.is_empty() {
            format!("/{rel_str}")
        } else {
            format!("{remote_root}/{rel_str}")
        };
        if let Some(idx) = remote.rfind('/') {
            let parent = &remote[..idx];
            if !parent.is_empty() {
                parents.insert(parent.to_string());
            }
        }
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        planned.push((path.clone(), remote, bytes));
    }

    let guard = expect_ref
        .map(|expect_ref| DriftGuard::prepare(local_root, expect_ref, &planned))
        .transpose()?;

    let mut plan = DeployPlan {
        profile: profile_name.to_string(),
        local_root: local_root.display().to_string(),
        remote_root: remote_root.clone(),
        dry_run,
        files_uploaded: 0,
        bytes_uploaded: 0,
        directories_created: parents.len(),
        skipped: Vec::new(),
        uploaded: Vec::with_capacity(planned.len()),
        drift_check: None,
    };

    if dry_run {
        if let Some(guard) = &guard {
            plan.drift_check = Some(guard.check_in_a_new_session(connect)?);
        }
        plan.uploaded = planned
            .into_iter()
            .map(|(local, remote, bytes)| UploadedFile {
                local: local.display().to_string(),
                remote,
                bytes,
            })
            .collect();
        plan.files_uploaded = plan.uploaded.len();
        plan.bytes_uploaded = plan.uploaded.iter().map(|f| f.bytes).sum();
        return Ok(plan);
    }

    // No files survived filtering — skip the network round trip entirely.
    if planned.is_empty() {
        plan.drift_check = guard.map(|guard| DriftCheck::without_targets(&guard.resolved));
        return Ok(plan);
    }

    let mut client = connect()?;

    if let Some(guard) = &guard {
        let check = drift::check_drift(&mut client, &guard.targets, &guard.resolved)?;
        let is_refused = check.refused;
        plan.drift_check = Some(check);
        if is_refused {
            // A refused run uploads nothing and creates nothing, so no directory counts.
            plan.directories_created = 0;
            client.quit();
            return Ok(plan);
        }
    }

    // Create the deepest parents — mkdir_p handles intermediates and ignores
    // "already exists" so duplicates are cheap.
    for parent in &parents {
        client.mkdir_p(parent)?;
    }

    for (index, (local, remote, _expected_bytes)) in planned.into_iter().enumerate() {
        // With a drift check, the bytes were read once for the check, and the upload reuses them.
        let written = match &guard {
            Some(guard) => {
                let mut source = Cursor::new(guard.targets[index].upload_bytes.as_slice());
                client
                    .put_reader(&remote, &mut source)
                    .with_context(|| format!("uploading {} -> {}", local.display(), remote))?
            }
            None => match File::open(&local) {
                Ok(mut f) => client
                    .put_reader(&remote, &mut f)
                    .with_context(|| format!("uploading {} -> {}", local.display(), remote))?,
                Err(e) => {
                    plan.skipped.push(format!("{}: {e}", local.display()));
                    continue;
                }
            },
        };
        plan.uploaded.push(UploadedFile {
            local: local.display().to_string(),
            remote,
            bytes: written,
        });
        plan.files_uploaded += 1;
        plan.bytes_uploaded += written;
    }

    client.quit();
    Ok(plan)
}

/// What `expect_ref` needs before the run may connect: the resolved commit, and every target with
/// its upload bytes read once.
struct DriftGuard {
    resolved: ResolvedRef,
    targets: Vec<DriftTarget>,
}

impl DriftGuard {
    /// Resolves the ref, reads every target's local bytes, and rejects a target whose path at
    /// the ref is not a regular file. Every failure is `InvalidArgs`, and none opens a connection.
    fn prepare(
        local_root: &Path,
        expect_ref: &str,
        planned: &[(PathBuf, String, u64)],
    ) -> std::result::Result<Self, DeployError> {
        let resolved = drift::resolve_expect_ref(local_root, expect_ref)?;
        let mut targets = Vec::with_capacity(planned.len());
        for (local, remote, _planned_bytes) in planned {
            let upload_bytes = std::fs::read(local).map_err(|error| {
                DeployError::InvalidArgs(format!("cannot read {}: {error}", local.display()))
            })?;
            targets.push(DriftTarget {
                remote_path: remote.clone(),
                repo_path: drift::repo_relative_path(&resolved, local)?,
                upload_bytes,
            });
        }
        drift::validate_expected_paths(&resolved, &targets)?;
        Ok(Self { resolved, targets })
    }

    /// Runs the check for a dry run: it connects, downloads, and disconnects, and never writes.
    /// With no targets there is nothing to download, so it does not connect.
    fn check_in_a_new_session<R: DeployRemote>(
        &self,
        connect: impl FnOnce() -> Result<R>,
    ) -> std::result::Result<DriftCheck, DeployError> {
        if self.targets.is_empty() {
            return Ok(DriftCheck::without_targets(&self.resolved));
        }
        let mut client = connect()?;
        let check = drift::check_drift(&mut client, &self.targets, &self.resolved)?;
        client.quit();
        Ok(check)
    }
}

fn walk_files(root: &Path, extra_ignore: &[String]) -> Result<Vec<PathBuf>> {
    let mut overrides = OverrideBuilder::new(root);
    for pat in extra_ignore {
        // Negation in the `ignore` crate's override DSL means *exclude*, so
        // turn "node_modules" into "!node_modules".
        overrides
            .add(&format!("!{pat}"))
            .with_context(|| format!("invalid ignore pattern '{pat}'"))?;
    }
    let overrides = overrides.build().context("building ignore overrides")?;

    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(false) // dotfiles like .htaccess are usually wanted
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .overrides(overrides)
        .build();

    let mut files = Vec::new();
    for entry in walker {
        let entry = entry.context("walking local_root")?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

fn path_to_posix(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch_deploy::RemoteFailure;
    use crate::drift::tests::{FakeRemote, TestRepo};
    use crate::drift::{DriftReason, DriftedFile};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    const REMOTE_ROOT: &str = "/home/test";

    /// Everything the fake server saw, shared with the test that owns the connection closure.
    #[derive(Default)]
    struct ServerState {
        remote: FakeRemote,
        stored: BTreeMap<String, Vec<u8>>,
        connections: usize,
    }

    impl ServerState {
        fn seed(&mut self, path: &str, bytes: &[u8]) {
            self.remote.files.insert(path.to_string(), bytes.to_vec());
        }

        fn writes(&self) -> Vec<&String> {
            self.remote
                .calls
                .iter()
                .filter(|call| call.starts_with("MKD ") || call.starts_with("STOR "))
                .collect()
        }

        fn downloads(&self) -> Vec<&String> {
            self.remote
                .calls
                .iter()
                .filter(|call| call.starts_with("RETR "))
                .collect()
        }

        fn position(&self, call_prefix: &str) -> Option<usize> {
            self.remote
                .calls
                .iter()
                .position(|call| call.starts_with(call_prefix))
        }
    }

    type SharedState = Rc<RefCell<ServerState>>;

    struct FakeDeployRemote(SharedState);

    impl DriftRemote for FakeDeployRemote {
        fn set_binary_mode(&mut self) -> std::result::Result<(), RemoteFailure> {
            self.0.borrow_mut().remote.set_binary_mode()
        }

        fn download_bytes(
            &mut self,
            path: &str,
        ) -> std::result::Result<Option<Vec<u8>>, RemoteFailure> {
            self.0.borrow_mut().remote.download_bytes(path)
        }
    }

    impl DeployRemote for FakeDeployRemote {
        fn mkdir_p(&mut self, path: &str) -> Result<()> {
            self.0.borrow_mut().remote.calls.push(format!("MKD {path}"));
            Ok(())
        }

        fn put_bytes(&mut self, remote_path: &str, bytes: &[u8]) -> Result<u64> {
            let mut state = self.0.borrow_mut();
            state.remote.calls.push(format!("STOR {remote_path}"));
            state
                .remote
                .files
                .insert(remote_path.to_string(), bytes.to_vec());
            state.stored.insert(remote_path.to_string(), bytes.to_vec());
            Ok(bytes.len() as u64)
        }

        fn put_reader<R: Read>(&mut self, remote_path: &str, reader: &mut R) -> Result<u64> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            self.put_bytes(remote_path, &bytes)
        }

        fn quit(self) {}
    }

    /// A connection closure that counts how many times the run connected.
    fn connector(state: &SharedState) -> impl FnOnce() -> Result<FakeDeployRemote> {
        let state = Rc::clone(state);
        move || {
            state.borrow_mut().connections += 1;
            Ok(FakeDeployRemote(state))
        }
    }

    /// A repository whose base commit holds `base <name>` for every file and whose head commit
    /// holds `head <name>`. The working tree is at head, and `local_root` is the repository.
    struct Site {
        repo: TestRepo,
        profile: Profile,
        base: String,
        head: String,
    }

    fn content(version: &str, name: &str) -> Vec<u8> {
        format!("{version} {name}\n").into_bytes()
    }

    fn remote_path(name: &str) -> String {
        format!("{REMOTE_ROOT}/{name}")
    }

    /// `.git` is always ignored, as a real profile that deploys a repository must do.
    fn profile_for(local_root: &Path, extra_ignore: Vec<String>) -> Profile {
        let mut ignore = vec![".git".to_string()];
        ignore.extend(extra_ignore);
        Profile {
            host: "unused.invalid".to_string(),
            port: 21,
            user: "unused".to_string(),
            remote_root: REMOTE_ROOT.to_string(),
            local_root: local_root.display().to_string(),
            passive: true,
            tls: false,
            accept_invalid_certs: false,
            ignore,
        }
    }

    fn site(names: &[&str]) -> Site {
        let repo = TestRepo::new();
        for name in names {
            repo.write(name, &content("base", name));
        }
        let base = repo.commit_all("base");
        for name in names {
            repo.write(name, &content("head", name));
        }
        let head = repo.commit_all("head");
        let profile = profile_for(repo.path(), Vec::new());
        Site {
            repo,
            profile,
            base,
            head,
        }
    }

    impl Site {
        fn seed_base_copies(&self, state: &SharedState, names: &[&str]) {
            for name in names {
                state
                    .borrow_mut()
                    .seed(&remote_path(name), &content("base", name));
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Tool {
        Directory,
        Commit,
    }

    fn run_tool(
        tool: Tool,
        site: &Site,
        state: &SharedState,
        dry_run: bool,
        expect_ref: Option<&str>,
    ) -> std::result::Result<DeployPlan, DeployError> {
        match tool {
            Tool::Directory => {
                deploy_with("qa", &site.profile, dry_run, expect_ref, connector(state))
            }
            Tool::Commit => deploy_commits_with(
                "qa",
                &site.profile,
                std::slice::from_ref(&site.head),
                dry_run,
                expect_ref,
                connector(state),
            ),
        }
    }

    fn new_state() -> SharedState {
        Rc::new(RefCell::new(ServerState::default()))
    }

    fn expect_invalid_args(result: std::result::Result<DeployPlan, DeployError>) -> String {
        match result {
            Err(DeployError::InvalidArgs(message)) => message,
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    const BOTH_TOOLS: [Tool; 2] = [Tool::Directory, Tool::Commit];

    #[test]
    fn existing_tool_is_invoked_without_expect_ref_uploads_as_before_with_no_download() {
        for tool in BOTH_TOOLS {
            let site = site(&["a.txt", "sub/d.txt"]);
            let state = new_state();

            let plan = run_tool(tool, &site, &state, false, None).expect("deploy should succeed");

            let state = state.borrow();
            assert_eq!(state.connections, 1, "{tool:?}");
            // A directory walk yields files in filesystem order, so compare the uploads as a set.
            let mut calls = state.remote.calls.clone();
            calls[2..].sort();
            assert_eq!(
                calls,
                vec![
                    format!("MKD {REMOTE_ROOT}"),
                    format!("MKD {REMOTE_ROOT}/sub"),
                    format!("STOR {}", remote_path("a.txt")),
                    format!("STOR {}", remote_path("sub/d.txt")),
                ],
                "{tool:?}: no TYPE I and no RETR without expect_ref"
            );
            assert_eq!(plan.files_uploaded, 2);
            assert_eq!(plan.directories_created, 2);
            assert_eq!(
                plan.bytes_uploaded,
                (content("head", "a.txt").len() + content("head", "sub/d.txt").len()) as u64
            );
            let json = serde_json::to_value(&plan).expect("plan should serialize");
            assert!(json.get("drift_check").is_none(), "{tool:?}: {json}");
            assert_eq!(
                state.stored[&remote_path("a.txt")],
                content("head", "a.txt")
            );
        }
    }

    #[test]
    fn expected_ref_01_omitted_expected_ref_keeps_overwrite_behavior() {
        for tool in BOTH_TOOLS {
            let site = site(&["c.txt"]);
            let state = new_state();
            // The server copy matches neither the working tree nor any committed version.
            state
                .borrow_mut()
                .seed(&remote_path("c.txt"), b"server-only edit\n");

            let plan = run_tool(tool, &site, &state, false, None).expect("deploy should succeed");

            assert_eq!(
                state.borrow().stored[&remote_path("c.txt")],
                content("head", "c.txt"),
                "{tool:?}"
            );
            assert!(plan.drift_check.is_none(), "{tool:?}");
            assert!(state.borrow().downloads().is_empty(), "{tool:?}");
        }
    }

    #[test]
    fn drift_dry_run_02_dry_run_without_expected_ref_stays_offline() {
        for tool in BOTH_TOOLS {
            let site = site(&["a.txt", "b.txt"]);
            let state = new_state();

            let plan = run_tool(tool, &site, &state, true, None).expect("dry run should succeed");

            assert_eq!(state.borrow().connections, 0, "{tool:?}");
            assert!(plan.drift_check.is_none(), "{tool:?}");
            assert_eq!(plan.uploaded.len(), 2, "{tool:?}");
        }
    }

    #[test]
    fn expected_ref_02_unresolvable_expected_ref_is_rejected() {
        for (tool, bad_ref) in [
            (Tool::Directory, "no-such-branch"),
            (Tool::Commit, "0000000000000000000000000000000000000000"),
        ] {
            for dry_run in [false, true] {
                let site = site(&["a.txt"]);
                let state = new_state();

                expect_invalid_args(run_tool(tool, &site, &state, dry_run, Some(bad_ref)));

                let state = state.borrow();
                assert_eq!(state.connections, 0, "{tool:?} dry_run={dry_run}");
                assert!(state.remote.calls.is_empty(), "{tool:?} dry_run={dry_run}");
            }
        }
    }

    #[test]
    fn expected_ref_03_upload_source_outside_a_repository() {
        let outside = tempfile::TempDir::new().expect("temp directory should be created");
        std::fs::write(outside.path().join("loose.txt"), b"loose\n")
            .expect("fixture should be written");
        let profile = profile_for(outside.path(), Vec::new());
        let state = new_state();

        let message = expect_invalid_args(deploy_with(
            "qa",
            &profile,
            false,
            Some("HEAD"),
            connector(&state),
        ));

        assert!(message.contains("Git worktree"), "{message}");
        assert_eq!(state.borrow().connections, 0);
    }

    #[test]
    fn expected_ref_04_expected_path_is_not_a_regular_file() {
        for tool in BOTH_TOOLS {
            let repo = TestRepo::new();
            repo.write("keep.txt", b"keep\n");
            repo.write("x/inner.txt", b"inner\n");
            let base = repo.commit_all("base");
            // At the base commit `x` is a directory. The working tree makes it a file.
            std::fs::remove_dir_all(repo.path().join("x")).expect("directory should be removed");
            repo.write("x", b"now a file\n");
            let head = repo.commit_all("head");
            let site = Site {
                profile: profile_for(repo.path(), Vec::new()),
                repo,
                base: base.clone(),
                head,
            };
            let state = new_state();

            let message = expect_invalid_args(run_tool(tool, &site, &state, false, Some(&base)));

            assert!(message.contains('x'), "{tool:?}: {message}");
            assert_eq!(state.borrow().connections, 0, "{tool:?}");
        }
    }

    #[test]
    fn expected_ref_05_unreadable_local_file() {
        let site = site(&["a.txt"]);
        // A directory in the target list cannot be read as a file, and works as an unreadable
        // file for a process that ignores permission bits.
        let unreadable = site.repo.path().join("unreadable");
        std::fs::create_dir(&unreadable).expect("directory should be created");
        let state = new_state();

        let message = expect_invalid_args(upload_files(
            connector(&state),
            "qa",
            &site.profile,
            &site.repo.path().canonicalize().expect("root exists"),
            vec![site.repo.path().join("a.txt"), unreadable],
            false,
            Some("HEAD"),
        ));

        assert!(message.contains("unreadable"), "{message}");
        assert_eq!(state.borrow().connections, 0);
    }

    #[test]
    fn drift_refusal_01_one_drifted_file_blocks_the_others() {
        let clean_names = ["a.txt", "b.txt", "sub/d.txt"];
        for tool in BOTH_TOOLS {
            for clean_count in [0, 3] {
                let clean = &clean_names[..clean_count];
                let mut names = clean.to_vec();
                names.push("c.txt");
                let site = site(&names);
                let state = new_state();
                site.seed_base_copies(&state, clean);
                state
                    .borrow_mut()
                    .seed(&remote_path("c.txt"), b"server-only edit\n");

                let plan = run_tool(tool, &site, &state, false, Some(&site.base))
                    .expect("a refusal is a response, not an error");

                let label = format!("{tool:?} with {clean_count} clean files");
                assert_eq!(plan.files_uploaded, 0, "{label}");
                assert_eq!(plan.bytes_uploaded, 0, "{label}");
                assert_eq!(plan.directories_created, 0, "{label}");
                assert!(plan.uploaded.is_empty(), "{label}");
                let check = plan.drift_check.expect("expect_ref adds a drift check");
                assert!(check.refused, "{label}");
                assert_eq!(check.checked, names.len(), "{label}");
                assert_eq!(check.resolved_commit, site.base, "{label}");
                assert_eq!(
                    check.drifted,
                    vec![DriftedFile {
                        remote_path: remote_path("c.txt"),
                        reason: DriftReason::ContentDiffers,
                    }],
                    "{label}"
                );
                let state = state.borrow();
                assert!(state.writes().is_empty(), "{label}: {:?}", state.writes());
                assert_eq!(state.connections, 1, "{label}");
            }
        }
    }

    #[test]
    fn drift_refusal_02_no_drift_uploads_normally() {
        let names = ["a.txt", "b.txt", "sub/d.txt"];
        for tool in BOTH_TOOLS {
            let site = site(&names);
            let plain_state = new_state();
            let plain = run_tool(tool, &site, &plain_state, false, None)
                .expect("the plain run should succeed");
            let guarded_state = new_state();
            site.seed_base_copies(&guarded_state, &names);

            let guarded = run_tool(tool, &site, &guarded_state, false, Some(&site.base))
                .expect("the guarded run should succeed");

            let check = guarded
                .drift_check
                .as_ref()
                .expect("expect_ref adds a check");
            assert!(!check.refused, "{tool:?}");
            assert!(check.drifted.is_empty(), "{tool:?}");
            assert_eq!(check.checked, names.len(), "{tool:?}");
            assert_eq!(guarded.files_uploaded, plain.files_uploaded, "{tool:?}");
            assert_eq!(guarded.bytes_uploaded, plain.bytes_uploaded, "{tool:?}");
            assert_eq!(
                guarded.directories_created, plain.directories_created,
                "{tool:?}"
            );
            let uploads = |plan: &DeployPlan| -> Vec<(String, u64)> {
                plan.uploaded
                    .iter()
                    .map(|file| (file.remote.clone(), file.bytes))
                    .collect()
            };
            assert_eq!(uploads(&guarded), uploads(&plain), "{tool:?}");
            let guarded_state = guarded_state.borrow();
            assert_eq!(
                guarded_state.stored,
                plain_state.borrow().stored,
                "{tool:?}: same bytes as a run without expect_ref"
            );
            // The check finishes before the run creates a directory or uploads a file.
            let last_download = guarded_state
                .remote
                .calls
                .iter()
                .rposition(|call| call.starts_with("RETR "))
                .expect("the check downloads server copies");
            let first_write = guarded_state
                .position("MKD ")
                .expect("the run creates directories");
            assert!(last_download < first_write, "{tool:?}");
            assert_eq!(guarded_state.position("TYPE I"), Some(0), "{tool:?}");
            assert_eq!(guarded_state.connections, 1, "{tool:?}");
        }
    }

    #[test]
    fn drift_dry_run_01_drift_is_reported_without_uploading() {
        let names = ["a.txt", "sub/d.txt", "c.txt"];
        for tool in BOTH_TOOLS {
            let site = site(&names);
            let offline_state = new_state();
            let offline = run_tool(tool, &site, &offline_state, true, None)
                .expect("the offline dry run should succeed");
            let state = new_state();
            site.seed_base_copies(&state, &["a.txt", "sub/d.txt"]);
            state
                .borrow_mut()
                .seed(&remote_path("c.txt"), b"server-only edit\n");

            let plan = run_tool(tool, &site, &state, true, Some(&site.base))
                .expect("the dry run should succeed");

            assert!(plan.dry_run, "{tool:?}");
            assert_eq!(plan.files_uploaded, offline.files_uploaded, "{tool:?}");
            assert_eq!(plan.bytes_uploaded, offline.bytes_uploaded, "{tool:?}");
            assert_eq!(
                plan.directories_created, offline.directories_created,
                "{tool:?}: a refused dry run keeps the normal dry-run counts"
            );
            assert_eq!(plan.uploaded.len(), names.len(), "{tool:?}");
            let check = plan.drift_check.expect("expect_ref adds a drift check");
            assert!(check.refused, "{tool:?}");
            assert_eq!(check.drifted.len(), 1, "{tool:?}");
            assert_eq!(check.drifted[0].remote_path, remote_path("c.txt"));
            let state = state.borrow();
            assert!(state.writes().is_empty(), "{tool:?}");
            assert_eq!(state.connections, 1, "{tool:?}");
        }
    }

    #[test]
    fn drift_classification_03_download_error_is_not_treated_as_missing() {
        for tool in BOTH_TOOLS {
            let site = site(&["a.txt", "b.txt", "sub/d.txt"]);
            let state = new_state();
            state
                .borrow_mut()
                .seed(&remote_path("a.txt"), b"server-only edit\n");
            state.borrow_mut().remote.failures.insert(
                remote_path("b.txt"),
                RemoteFailure::connection_lost("connection reset"),
            );

            let error = run_tool(tool, &site, &state, false, Some(&site.base))
                .expect_err("a download failure fails the run");

            assert!(
                error.to_string().contains(&remote_path("b.txt")),
                "{tool:?}: {error}"
            );
            assert!(matches!(error, DeployError::Other(_)), "{tool:?}");
            assert!(state.borrow().writes().is_empty(), "{tool:?}");
        }
    }

    #[test]
    fn drift_check_finds_the_expected_copy_by_repository_path_below_the_local_root() {
        let repo = TestRepo::new();
        repo.write("site/a.txt", b"base a\n");
        let base = repo.commit_all("base");
        repo.write("site/a.txt", b"head a\n");
        let head = repo.commit_all("head");
        let site = Site {
            profile: profile_for(&repo.path().join("site"), Vec::new()),
            repo,
            base: base.clone(),
            head,
        };
        let state = new_state();
        // The server holds the base copy, so the file is clean only when the expected copy is
        // looked up as `site/a.txt`, the path inside the repository.
        state.borrow_mut().seed(&remote_path("a.txt"), b"base a\n");

        // Commit deployment uploads nothing below a subdirectory local root (a known bug that
        // predates the drift guard), so only directory deployment is exercised here.
        let plan = run_tool(Tool::Directory, &site, &state, false, Some(&base))
            .expect("the guarded run should succeed");

        assert!(!plan.drift_check.expect("check present").refused);
        assert_eq!(
            state.borrow().stored[&remote_path("a.txt")],
            b"head a\n".to_vec()
        );
    }

    #[test]
    fn ignored_and_absent_files_are_not_drift_targets() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"base a\n");
        repo.write("skip.log", b"base log\n");
        repo.write("gone.txt", b"base gone\n");
        let base = repo.commit_all("base");
        repo.write("a.txt", b"head a\n");
        repo.write("skip.log", b"head log\n");
        std::fs::remove_file(repo.path().join("gone.txt")).expect("file should be removed");
        let head = repo.commit_all("head");
        let site = Site {
            profile: profile_for(repo.path(), vec!["*.log".to_string()]),
            repo,
            base: base.clone(),
            head,
        };
        for tool in BOTH_TOOLS {
            let state = new_state();
            state.borrow_mut().seed(&remote_path("a.txt"), b"base a\n");

            let plan = run_tool(tool, &site, &state, false, Some(&base))
                .expect("the guarded run should succeed");

            let check = plan.drift_check.expect("check present");
            assert_eq!(check.checked, 1, "{tool:?}");
            assert_eq!(
                state.borrow().downloads(),
                vec![&format!("RETR {}", remote_path("a.txt"))],
                "{tool:?}"
            );
        }
    }

    #[test]
    fn a_run_with_no_target_files_still_validates_the_ref_and_skips_the_connection() {
        let repo = TestRepo::new();
        repo.write("a.txt", b"base a\n");
        let base = repo.commit_all("base");
        std::fs::remove_file(repo.path().join("a.txt")).expect("file should be removed");
        let head = repo.commit_all("head");
        let site = Site {
            profile: profile_for(repo.path(), Vec::new()),
            repo,
            base: base.clone(),
            head,
        };
        for dry_run in [false, true] {
            let state = new_state();
            let plan = run_tool(Tool::Commit, &site, &state, dry_run, Some(&base))
                .expect("an empty run should succeed");

            let check = plan.drift_check.expect("check present");
            assert_eq!(check.checked, 0, "dry_run={dry_run}");
            assert!(!check.refused, "dry_run={dry_run}");
            assert_eq!(state.borrow().connections, 0, "dry_run={dry_run}");

            expect_invalid_args(run_tool(
                Tool::Commit,
                &site,
                &state,
                dry_run,
                Some("no-such-branch"),
            ));
        }
    }

    /// A repository with `Mails.php` at three versions: base, head (HEAD), and an uncommitted
    /// working-tree edit.
    fn single_file_site() -> Site {
        let site = site(&["Mails.php"]);
        site.repo.write("Mails.php", b"working-tree edit\n");
        site
    }

    fn upload_single(
        site: &Site,
        state: &SharedState,
        before_changes: bool,
        expect_ref: Option<&str>,
    ) -> std::result::Result<UploadFileOutcome, DeployError> {
        let local = site.repo.path().join("Mails.php");
        let remote = remote_path("Mails.php");
        upload_file_with(
            &UploadFileRequest {
                local_path: &local.display().to_string(),
                remote_path: &remote,
                before_changes,
                expect_ref,
            },
            connector(state),
        )
    }

    fn expect_invalid_upload(
        result: std::result::Result<UploadFileOutcome, DeployError>,
    ) -> String {
        match result {
            Err(DeployError::InvalidArgs(message)) => message,
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn existing_tool_is_invoked_single_file_upload_without_expect_ref_downloads_nothing() {
        for before_changes in [false, true] {
            let site = single_file_site();
            let state = new_state();

            let outcome = upload_single(&site, &state, before_changes, None)
                .expect("the upload should succeed");

            let expected: &[u8] = if before_changes {
                b"head Mails.php\n"
            } else {
                b"working-tree edit\n"
            };
            let state = state.borrow();
            assert_eq!(outcome.bytes, expected.len() as u64);
            assert!(outcome.drift_check.is_none());
            assert_eq!(state.stored[&remote_path("Mails.php")], expected);
            assert_eq!(
                state.remote.calls,
                vec![format!("STOR {}", remote_path("Mails.php"))],
                "before_changes={before_changes}: no TYPE I and no RETR without expect_ref"
            );
        }
    }

    #[test]
    fn expected_ref_01_single_file_upload_overwrites_without_expect_ref() {
        let site = single_file_site();
        let state = new_state();
        state
            .borrow_mut()
            .seed(&remote_path("Mails.php"), b"server-only edit\n");

        let outcome = upload_single(&site, &state, false, None).expect("the upload should succeed");

        assert!(outcome.drift_check.is_none());
        assert_eq!(
            state.borrow().stored[&remote_path("Mails.php")],
            b"working-tree edit\n".to_vec()
        );
    }

    #[test]
    fn expected_ref_02_single_file_unresolvable_expected_ref_is_rejected() {
        let site = single_file_site();
        let state = new_state();

        expect_invalid_upload(upload_single(&site, &state, false, Some("no-such-branch")));

        assert_eq!(state.borrow().connections, 0);
        assert!(state.borrow().remote.calls.is_empty());
    }

    #[test]
    fn expected_ref_03_single_file_upload_source_outside_a_repository() {
        let outside = tempfile::TempDir::new().expect("temp directory should be created");
        let loose = outside.path().join("loose.txt");
        std::fs::write(&loose, b"loose\n").expect("fixture should be written");
        let state = new_state();

        for before_changes in [false, true] {
            let message = expect_invalid_upload(upload_file_with(
                &UploadFileRequest {
                    local_path: &loose.display().to_string(),
                    remote_path: "/home/test/loose.txt",
                    before_changes,
                    expect_ref: Some("HEAD"),
                },
                connector(&state),
            ));

            assert!(message.contains("Git worktree"), "{message}");
        }
        assert_eq!(state.borrow().connections, 0);
    }

    #[test]
    fn expected_ref_04_single_file_expected_path_is_not_a_regular_file() {
        let repo = TestRepo::new();
        repo.write("Mails.php/inner.txt", b"inner\n");
        let base = repo.commit_all("base");
        std::fs::remove_dir_all(repo.path().join("Mails.php"))
            .expect("directory should be removed");
        repo.write("Mails.php", b"now a file\n");
        let head = repo.commit_all("head");
        let site = Site {
            profile: profile_for(repo.path(), Vec::new()),
            repo,
            base: base.clone(),
            head,
        };
        let state = new_state();

        let message = expect_invalid_upload(upload_single(&site, &state, false, Some(&base)));

        assert!(message.contains("Mails.php"), "{message}");
        assert_eq!(state.borrow().connections, 0);
    }

    #[test]
    fn expected_ref_05_single_file_unreadable_local_file() {
        let site = single_file_site();
        let state = new_state();
        let directory = site.repo.path().join("folder");
        std::fs::create_dir(&directory).expect("directory should be created");

        for local_path in [directory, site.repo.path().join("missing.php")] {
            let message = expect_invalid_upload(upload_file_with(
                &UploadFileRequest {
                    local_path: &local_path.display().to_string(),
                    remote_path: "/home/test/x",
                    before_changes: false,
                    expect_ref: Some("HEAD"),
                },
                connector(&state),
            ));

            assert!(
                message.contains(&local_path.display().to_string()),
                "{message}"
            );
        }
        assert_eq!(state.borrow().connections, 0);
    }

    #[test]
    fn drift_refusal_03_single_file_upload_refused() {
        let site = single_file_site();
        let state = new_state();
        // The server copy differs from the base copy and from the working-tree copy.
        state
            .borrow_mut()
            .seed(&remote_path("Mails.php"), b"server-only edit\n");

        let outcome =
            upload_single(&site, &state, false, Some(&site.base)).expect("a refusal is a response");

        let check = outcome.drift_check.expect("expect_ref adds a drift check");
        assert_eq!(outcome.bytes, 0);
        assert!(check.refused);
        assert_eq!(
            check.drifted,
            vec![DriftedFile {
                remote_path: remote_path("Mails.php"),
                reason: DriftReason::ContentDiffers,
            }]
        );
        let state = state.borrow();
        assert!(state.stored.is_empty());
        assert_eq!(
            state.remote.files[&remote_path("Mails.php")],
            b"server-only edit\n".to_vec()
        );
    }

    #[test]
    fn drift_classification_02_single_file_upload_of_the_committed_version() {
        let site = single_file_site();
        let state = new_state();
        // The server holds the HEAD version. The expected copy is the older base version, so the
        // file is clean only because the upload copy is HEAD, not the working-tree edit.
        state
            .borrow_mut()
            .seed(&remote_path("Mails.php"), b"head Mails.php\n");

        let outcome = upload_single(&site, &state, true, Some(&site.base))
            .expect("the upload should succeed");

        let check = outcome.drift_check.expect("expect_ref adds a drift check");
        assert!(!check.refused);
        assert!(check.drifted.is_empty());
        assert_eq!(outcome.bytes, b"head Mails.php\n".len() as u64);
    }

    #[test]
    fn drift_refusal_02_single_file_upload_without_drift_uploads_the_working_tree() {
        let site = single_file_site();
        let state = new_state();
        state
            .borrow_mut()
            .seed(&remote_path("Mails.php"), &content("base", "Mails.php"));

        let outcome = upload_single(&site, &state, false, Some(&site.base))
            .expect("the upload should succeed");

        let check = outcome.drift_check.expect("expect_ref adds a drift check");
        assert!(!check.refused);
        assert_eq!(check.checked, 1);
        assert_eq!(outcome.bytes, b"working-tree edit\n".len() as u64);
        let state = state.borrow();
        assert_eq!(
            state.remote.calls,
            vec![
                "TYPE I".to_string(),
                format!("RETR {}", remote_path("Mails.php")),
                format!("STOR {}", remote_path("Mails.php")),
            ]
        );
        assert_eq!(state.connections, 1);
    }

    #[test]
    fn drift_classification_03_single_file_download_error_uploads_nothing() {
        let site = single_file_site();
        let state = new_state();
        state.borrow_mut().remote.failures.insert(
            remote_path("Mails.php"),
            RemoteFailure::operation("550 Failed to open file"),
        );

        let error = upload_single(&site, &state, false, Some(&site.base))
            .expect_err("a download failure fails the upload");

        assert!(
            error.to_string().contains(&remote_path("Mails.php")),
            "{error}"
        );
        assert!(state.borrow().writes().is_empty());
    }

    #[test]
    fn single_file_expected_copy_is_found_by_the_path_inside_the_repository() {
        let repo = TestRepo::new();
        repo.write("app/models/Mails.php", b"base\n");
        let base = repo.commit_all("base");
        repo.write("app/models/Mails.php", b"edited\n");
        let state = new_state();
        // The server holds the base copy, so the file is clean only when the expected copy is
        // looked up as `app/models/Mails.php`, not as the bare file name.
        state
            .borrow_mut()
            .seed(&remote_path("Mails.php"), b"base\n");
        let local = repo.path().join("app/models/Mails.php");
        let remote = remote_path("Mails.php");

        let outcome = upload_file_with(
            &UploadFileRequest {
                local_path: &local.display().to_string(),
                remote_path: &remote,
                before_changes: false,
                expect_ref: Some(&base),
            },
            connector(&state),
        )
        .expect("the upload should succeed");

        assert!(!outcome.drift_check.expect("check present").refused);
        assert_eq!(outcome.bytes, b"edited\n".len() as u64);
    }
}
