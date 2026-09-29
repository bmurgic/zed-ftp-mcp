//! The per-file merge decision for merge-mode branch deployment.
//!
//! `decide` applies spec rules 2 through 8 to the base, head, and server copies of one file.
//! Rule 1 (`unchanged_in_range`) compares blob IDs, so the executor decides it before any download.

use super::git::{configure_git_command, git_spawn_error};
use super::BranchDeployError;
use schemars::JsonSchema;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// `git merge-file` reports the number of conflicts as its exit status, capped at 127.
const MAX_CONFLICT_EXIT_STATUS: i32 = 127;
static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    TextConflict,
    BinaryChanged,
    DeletedOnServer,
    AddedOnBoth,
}

impl ConflictReason {
    /// The `merge`-stage failure text. It starts with the reason as the manifest spells it.
    pub fn failure_message(self) -> &'static str {
        match self {
            Self::TextConflict => {
                "text_conflict: the server copy and head changed nearby lines since base"
            }
            Self::BinaryChanged => {
                "binary_changed: a version is binary and the server copy differs from base and head"
            }
            Self::DeletedOnServer => {
                "deleted_on_server: the file exists at base but is missing on the server"
            }
            Self::AddedOnBoth => {
                "added_on_both: the file is new in head and the server has a different copy"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeDecision {
    AlreadyDeployed,
    FastForward,
    Merged(Vec<u8>),
    NewFile,
    Conflict {
        reason: ConflictReason,
        marked_text: Option<Vec<u8>>,
    },
}

pub fn decide(
    base: Option<&[u8]>,
    head: &[u8],
    server: Option<&[u8]>,
) -> Result<MergeDecision, BranchDeployError> {
    decide_in(&std::env::temp_dir(), base, head, server)
}

pub(super) fn decide_in(
    workspace_parent: &Path,
    base: Option<&[u8]>,
    head: &[u8],
    server: Option<&[u8]>,
) -> Result<MergeDecision, BranchDeployError> {
    // Rule 2.
    if server == Some(head) {
        return Ok(MergeDecision::AlreadyDeployed);
    }
    match (base, server) {
        (Some(base), Some(server)) => decide_server_changed(workspace_parent, base, head, server),
        // Rule 6.
        (Some(_), None) => Ok(conflict_without_text(ConflictReason::DeletedOnServer)),
        // Rule 7.
        (None, None) => Ok(MergeDecision::NewFile),
        // Rule 8.
        (None, Some(_)) => Ok(conflict_without_text(ConflictReason::AddedOnBoth)),
    }
}

/// Rules 3 to 5, for a file whose base and server copies both exist and the server copy is not
/// the head blob.
fn decide_server_changed(
    workspace_parent: &Path,
    base: &[u8],
    head: &[u8],
    server: &[u8],
) -> Result<MergeDecision, BranchDeployError> {
    // Rule 3.
    if server == base {
        return Ok(MergeDecision::FastForward);
    }
    // Rule 5.
    if [base, head, server].into_iter().any(contains_nul_byte) {
        return Ok(conflict_without_text(ConflictReason::BinaryChanged));
    }
    // Rule 4.
    merge_text(workspace_parent, base, head, server)
}

fn conflict_without_text(reason: ConflictReason) -> MergeDecision {
    MergeDecision::Conflict {
        reason,
        marked_text: None,
    }
}

/// A file is binary when a NUL byte appears anywhere. Git checks only the first 8,000 bytes,
/// which would let a late NUL byte through into a text merge.
fn contains_nul_byte(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn merge_text(
    workspace_parent: &Path,
    base: &[u8],
    head: &[u8],
    server: &[u8],
) -> Result<MergeDecision, BranchDeployError> {
    let workspace = MergeWorkspace::create(workspace_parent)?;
    workspace.write_inputs(base, head, server)?;
    let output = run_merge_file(&workspace.path)?;
    interpret_merge_file_output(output.status.code(), output.stdout, &output.stderr)
}

fn run_merge_file(workspace: &Path) -> Result<Output, BranchDeployError> {
    let mut command = Command::new("git");
    configure_git_command(&mut command, workspace);
    // The server copy is the "current" file, so its layout wins wherever both sides agree.
    command
        .args([
            "merge-file",
            "-p",
            "--diff3",
            "-L",
            "server",
            "-L",
            "base",
            "-L",
            "head",
            "server",
            "base",
            "head",
        ])
        .output()
        .map_err(|error| git_spawn_error(error, "spawning git merge-file"))
}

pub(super) fn interpret_merge_file_output(
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: &[u8],
) -> Result<MergeDecision, BranchDeployError> {
    match exit_code {
        Some(0) => Ok(MergeDecision::Merged(stdout)),
        Some(conflicts) if (1..=MAX_CONFLICT_EXIT_STATUS).contains(&conflicts) => {
            Ok(MergeDecision::Conflict {
                reason: ConflictReason::TextConflict,
                marked_text: Some(stdout),
            })
        }
        _ => Err(BranchDeployError::Other(anyhow::anyhow!(
            "git merge-file failed with status {exit_code:?}: {}",
            String::from_utf8_lossy(stderr).trim()
        ))),
    }
}

/// A private directory that holds the three merge inputs and is removed on every exit path.
struct MergeWorkspace {
    path: PathBuf,
}

impl MergeWorkspace {
    fn create(parent: &Path) -> Result<Self, BranchDeployError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let sequence = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "zed-ftp-merge-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&path).map_err(|error| {
            BranchDeployError::Other(
                anyhow::Error::from(error).context("creating the merge workspace"),
            )
        })?;
        Ok(Self { path })
    }

    /// Writes the files that `run_merge_file` names: `server`, `base`, and `head`.
    fn write_inputs(
        &self,
        base: &[u8],
        head: &[u8],
        server: &[u8],
    ) -> Result<(), BranchDeployError> {
        for (name, bytes) in [("server", server), ("base", base), ("head", head)] {
            fs::write(self.path.join(name), bytes).map_err(|error| {
                BranchDeployError::Other(
                    anyhow::Error::from(error).context(format!("writing the {name} merge input")),
                )
            })?;
        }
        Ok(())
    }
}

impl Drop for MergeWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
