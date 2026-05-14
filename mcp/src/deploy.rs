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
use crate::ftp::FtpClient;
use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeployPlan {
    pub profile: String,
    pub local_root: String,
    pub remote_root: String,
    pub dry_run: bool,
    pub files_uploaded: usize,
    pub bytes_uploaded: u64,
    pub directories_created: usize,
    pub skipped: Vec<String>,
    pub uploaded: Vec<UploadedFile>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UploadedFile {
    pub local: String,
    pub remote: String,
    pub bytes: u64,
}

/// Routable error for `deploy_commits` so the tool layer can return
/// `invalid_params` for user-fixable mistakes (bad SHA, missing git) and
/// `internal_error` for everything else.
#[derive(thiserror::Error, Debug)]
pub enum DeployCommitsError {
    #[error("{0}")]
    InvalidArgs(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Full-tree deploy: walk `local_root` honoring .gitignore and per-profile
/// ignore patterns, then upload everything that survives the filter.
pub fn deploy(profile_name: &str, profile: &Profile, dry_run: bool) -> Result<DeployPlan> {
    let local_root = canon_local_root(profile)?;
    let files = walk_files(&local_root, &profile.ignore)?;
    upload_files(profile_name, profile, &local_root, files, dry_run)
}

/// Commit-scoped deploy: union the file lists from each commit's
/// `git diff-tree` and upload only those files (current working-tree state).
pub fn deploy_commits(
    profile_name: &str,
    profile: &Profile,
    commits: &[String],
    dry_run: bool,
) -> std::result::Result<DeployPlan, DeployCommitsError> {
    if commits.is_empty() {
        return Err(DeployCommitsError::InvalidArgs(
            "commits list is empty".to_string(),
        ));
    }

    let local_root = canon_local_root(profile).map_err(DeployCommitsError::Other)?;

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
            .map_err(DeployCommitsError::Other)?;
    }
    let overrides = overrides
        .build()
        .context("building ignore overrides")
        .map_err(DeployCommitsError::Other)?;

    // Belt-and-suspenders: even with --diff-filter=ACMRT, rename sources or
    // race-deleted files might not exist on disk. Skip those silently.
    let files: Vec<PathBuf> = rel_paths
        .into_iter()
        .filter(|rel| overrides.matched(rel, false).is_none())
        .map(|rel| local_root.join(rel))
        .filter(|p| p.is_file())
        .collect();

    Ok(upload_files(profile_name, profile, &local_root, files, dry_run)?)
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
) -> std::result::Result<Vec<PathBuf>, DeployCommitsError> {
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
                DeployCommitsError::InvalidArgs(
                    "`git` was not found on PATH; install git or add it to your PATH".to_string(),
                )
            } else {
                DeployCommitsError::Other(anyhow::Error::from(e).context("spawning git diff-tree"))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("git diff-tree failed for '{sha}' (exit {})", output.status)
        } else {
            format!("git diff-tree failed for '{sha}': {stderr}")
        };
        return Err(DeployCommitsError::InvalidArgs(msg));
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
pub(crate) fn upload_files(
    profile_name: &str,
    profile: &Profile,
    local_root: &Path,
    files: Vec<PathBuf>,
    dry_run: bool,
) -> Result<DeployPlan> {
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
    };

    if dry_run {
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
        return Ok(plan);
    }

    let mut client = FtpClient::connect(profile_name, profile)?;

    // Create the deepest parents — mkdir_p handles intermediates and ignores
    // "already exists" so duplicates are cheap.
    for parent in &parents {
        client.mkdir_p(parent)?;
    }

    for (local, remote, _expected_bytes) in planned {
        match File::open(&local) {
            Ok(mut f) => {
                let written = client
                    .put_reader(&remote, &mut f)
                    .with_context(|| format!("uploading {} -> {}", local.display(), remote))?;
                plan.uploaded.push(UploadedFile {
                    local: local.display().to_string(),
                    remote,
                    bytes: written,
                });
                plan.files_uploaded += 1;
                plan.bytes_uploaded += written;
            }
            Err(e) => {
                plan.skipped.push(format!("{}: {e}", local.display()));
            }
        }
    }

    client.quit();
    Ok(plan)
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
