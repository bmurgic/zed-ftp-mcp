//! The per-file merge decision for merge-mode branch deployment.
//!
//! `decide` applies spec rules 2 through 8 to the base, head, and server copies of one file.
//! Rule 1 (`unchanged_in_range`) compares blob IDs, so the executor decides it before any download.

use super::git::configure_git_command;
use super::BranchDeployError;
use schemars::JsonSchema;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The longest `marked_text` a manifest carries.
const MAX_MARKED_TEXT_BYTES: usize = 65_536;
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
    if server == Some(head) {
        return Ok(MergeDecision::AlreadyDeployed);
    }
    match (base, server) {
        (Some(base), Some(server)) if server == base => Ok(MergeDecision::FastForward),
        (Some(base), Some(server)) => {
            if [base, head, server].into_iter().any(contains_nul_byte) {
                return Ok(MergeDecision::Conflict {
                    reason: ConflictReason::BinaryChanged,
                    marked_text: None,
                });
            }
            merge_text(workspace_parent, base, head, server)
        }
        (Some(_), None) => Ok(MergeDecision::Conflict {
            reason: ConflictReason::DeletedOnServer,
            marked_text: None,
        }),
        (None, None) => Ok(MergeDecision::NewFile),
        (None, Some(_)) => Ok(MergeDecision::Conflict {
            reason: ConflictReason::AddedOnBoth,
            marked_text: None,
        }),
    }
}

/// Converts conflict-marked bytes to manifest text. Invalid UTF-8 becomes U+FFFD, and the text
/// is cut to at most 65,536 bytes without splitting a character. The flag reports a cut.
pub fn marked_text_for_manifest(marked_text: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(marked_text);
    if text.len() <= MAX_MARKED_TEXT_BYTES {
        return (text.into_owned(), false);
    }
    let mut cut = MAX_MARKED_TEXT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), true)
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
    for (name, bytes) in [("server", server), ("base", base), ("head", head)] {
        fs::write(workspace.path.join(name), bytes).map_err(|error| {
            BranchDeployError::Other(
                anyhow::Error::from(error).context(format!("writing the {name} merge input")),
            )
        })?;
    }

    let mut command = Command::new("git");
    configure_git_command(&mut command, &workspace.path);
    // The server copy is the "current" file, so its layout wins wherever both sides agree.
    let output = command
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
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BranchDeployError::InvalidArgs("`git` was not found on PATH".to_string())
            } else {
                BranchDeployError::Other(
                    anyhow::Error::from(error).context("spawning git merge-file"),
                )
            }
        })?;
    interpret_merge_file_output(output.status.code(), output.stdout, &output.stderr)
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
}

impl Drop for MergeWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
