use super::{
    map_remote_path, BranchDeployError, BranchDeployPlan, DeletedPathResult, DeletedPathStatus,
    DeployBranchRequest, FailureRecord, PlannedUpload, RepositorySummary, RequestedAndResolvedRef,
    ResolvedRefs,
};
use crate::config::Profile;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GIT_CONFIG_DISABLED_PATH: &str = "/dev/null";
const GIT_OPTIONAL_LOCKS_DISABLED: &str = "0";
const GIT_SAFE_PAGER: &str = "cat";

#[derive(Debug)]
struct TreeEntry {
    object_type: String,
    object_id: String,
    bytes: u64,
}

pub(super) fn plan_branch(
    request: &DeployBranchRequest,
    profile: &Profile,
) -> Result<BranchDeployPlan, BranchDeployError> {
    let repository_root = validate_repository_root(&request.repo_root)?;
    let base_commit = resolve_commit(&repository_root, &request.base_ref)?;
    let head_commit = resolve_commit(&repository_root, &request.head_ref)?;
    let commits = range_commits(&repository_root, &base_commit, &head_commit)?;
    let touched_paths = touched_paths(&repository_root, &commits)?;
    let head_tree = head_tree(&repository_root, &head_commit)?;
    let dirty = !run_git(
        &repository_root,
        ["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )?
    .stdout
    .is_empty();

    let surviving_blobs: BTreeSet<Vec<u8>> = head_tree
        .iter()
        .filter_map(|(path, entry)| (entry.object_type == "blob").then_some(path.clone()))
        .collect();
    let mut uploads = Vec::new();
    let mut deleted = Vec::new();
    let mut failures = Vec::new();

    for path in &touched_paths {
        match head_tree.get(path) {
            Some(entry) if entry.object_type == "blob" => {
                match map_remote_path(&profile.remote_root, path) {
                    Ok((git_path, remote_path)) => uploads.push(PlannedUpload {
                        git_path,
                        remote_path,
                        object_id: entry.object_id.clone(),
                        bytes: entry.bytes,
                    }),
                    Err(error) => failures.push(planning_failure(path, error.to_string())),
                }
            }
            Some(entry) => failures.push(planning_failure(
                path,
                format!("head entry is not a deployable blob: {}", entry.object_type),
            )),
            None => match map_remote_path(&profile.remote_root, path) {
                Ok((git_path, _)) => {
                    let status = if surviving_blobs
                        .iter()
                        .any(|surviving| ascii_fold(surviving) == ascii_fold(path))
                    {
                        DeletedPathStatus::BlockedCaseCollision
                    } else {
                        DeletedPathStatus::RequiresExplicitCall
                    };
                    deleted.push(DeletedPathResult { git_path, status });
                }
                Err(error) => failures.push(planning_failure(path, error.to_string())),
            },
        }
    }

    Ok(BranchDeployPlan {
        profile: request.profile.clone(),
        repository: RepositorySummary {
            root: repository_root.display().to_string(),
            dirty,
        },
        refs: ResolvedRefs {
            base: RequestedAndResolvedRef {
                requested: request.base_ref.clone(),
                commit: base_commit,
            },
            head: RequestedAndResolvedRef {
                requested: request.head_ref.clone(),
                commit: head_commit,
            },
        },
        commits,
        touched_paths: touched_paths.len(),
        uploads,
        deleted,
        failures,
    })
}

fn validate_repository_root(requested_root: &str) -> Result<PathBuf, BranchDeployError> {
    let requested = Path::new(requested_root);
    if !requested.is_absolute() {
        return Err(BranchDeployError::InvalidArgs(
            "repository root must be an absolute path".to_string(),
        ));
    }
    let normalized = requested.canonicalize().map_err(|error| {
        BranchDeployError::InvalidArgs(format!(
            "repository root '{requested_root}' cannot be resolved: {error}"
        ))
    })?;
    let output = run_git(&normalized, ["rev-parse", "--show-toplevel"])?;
    let top_level = output_path(&output, "git rev-parse --show-toplevel")?;
    let top_level = PathBuf::from(top_level).canonicalize().map_err(|error| {
        BranchDeployError::InvalidArgs(format!("Git worktree root cannot be resolved: {error}"))
    })?;

    if normalized != top_level {
        return Err(BranchDeployError::InvalidArgs(format!(
            "repository root '{}' is not the selected worktree root '{}'",
            normalized.display(),
            top_level.display()
        )));
    }
    Ok(top_level)
}

fn resolve_commit(repository_root: &Path, reference: &str) -> Result<String, BranchDeployError> {
    let expression = format!("{reference}^{{commit}}");
    let output = run_git(
        repository_root,
        ["rev-parse", "--verify", expression.as_str()],
    )?;
    let commit = output_text(&output, "git rev-parse --verify")?;
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BranchDeployError::Other(anyhow::anyhow!(
            "git returned an invalid commit identifier '{commit}'"
        )));
    }
    Ok(commit)
}

fn range_commits(
    repository_root: &Path,
    base_commit: &str,
    head_commit: &str,
) -> Result<Vec<String>, BranchDeployError> {
    let range = format!("{base_commit}..{head_commit}");
    let output = run_git(
        repository_root,
        ["rev-list", "--reverse", "--topo-order", range.as_str()],
    )?;
    output_text(&output, "git rev-list").map(|text| text.lines().map(str::to_string).collect())
}

fn touched_paths(
    repository_root: &Path,
    commits: &[String],
) -> Result<BTreeSet<Vec<u8>>, BranchDeployError> {
    let mut touched = BTreeSet::new();
    for commit in commits {
        let parents = run_git(
            repository_root,
            ["rev-list", "--parents", "-n", "1", commit],
        )?;
        let parent_output = output_text(&parents, "git rev-list --parents")?;
        let parent_fields: Vec<&str> = parent_output.split_whitespace().collect();
        if parent_fields.first().copied() != Some(commit.as_str()) {
            return Err(BranchDeployError::Other(anyhow::anyhow!(
                "git rev-list returned unexpected commit parents for {commit}"
            )));
        }

        let diff = if let Some(first_parent) = parent_fields.get(1) {
            run_git(
                repository_root,
                [
                    "diff-tree",
                    "--no-commit-id",
                    "-r",
                    "--name-status",
                    "-z",
                    "--no-renames",
                    first_parent,
                    commit,
                ],
            )?
        } else {
            run_git(
                repository_root,
                [
                    "diff-tree",
                    "--root",
                    "--no-commit-id",
                    "-r",
                    "--name-status",
                    "-z",
                    "--no-renames",
                    commit,
                ],
            )?
        };

        for path in parse_name_status(&diff.stdout)? {
            touched.insert(path);
        }
    }
    Ok(touched)
}

fn head_tree(
    repository_root: &Path,
    head_commit: &str,
) -> Result<BTreeMap<Vec<u8>, TreeEntry>, BranchDeployError> {
    let output = run_git(
        repository_root,
        ["ls-tree", "-rz", "-l", "--full-tree", head_commit],
    )?;
    let mut tree = BTreeMap::new();

    for record in output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
    {
        let tab_index = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| {
                BranchDeployError::Other(anyhow::anyhow!("malformed git ls-tree record"))
            })?;
        let (metadata, path_with_separator) = record.split_at(tab_index);
        let path = &path_with_separator[1..];
        let fields: Vec<&[u8]> = metadata
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect();
        if fields.len() != 4 {
            return Err(BranchDeployError::Other(anyhow::anyhow!(
                "malformed git ls-tree metadata"
            )));
        }
        let object_type = std::str::from_utf8(fields[1]).map_err(|_| {
            BranchDeployError::Other(anyhow::anyhow!("git ls-tree returned a non-UTF-8 type"))
        })?;
        let object_id = std::str::from_utf8(fields[2]).map_err(|_| {
            BranchDeployError::Other(anyhow::anyhow!(
                "git ls-tree returned a non-UTF-8 object ID"
            ))
        })?;
        let bytes = std::str::from_utf8(fields[3])
            .ok()
            .and_then(|size| size.parse::<u64>().ok())
            .unwrap_or(0);
        tree.insert(
            path.to_vec(),
            TreeEntry {
                object_type: object_type.to_string(),
                object_id: object_id.to_string(),
                bytes,
            },
        );
    }
    Ok(tree)
}

fn parse_name_status(output: &[u8]) -> Result<Vec<Vec<u8>>, BranchDeployError> {
    let mut records = output
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty());
    let mut paths = Vec::new();
    while let Some(status) = records.next() {
        if status.is_empty() {
            return Err(BranchDeployError::Other(anyhow::anyhow!(
                "git diff-tree returned an empty status"
            )));
        }
        let path = records.next().ok_or_else(|| {
            BranchDeployError::Other(anyhow::anyhow!(
                "git diff-tree returned a status without a path"
            ))
        })?;
        paths.push(path.to_vec());
    }
    Ok(paths)
}

fn planning_failure(path: &[u8], error: String) -> FailureRecord {
    FailureRecord {
        stage: "planning".to_string(),
        git_path: String::from_utf8(path.to_vec()).ok(),
        error,
    }
}

fn ascii_fold(path: &[u8]) -> Vec<u8> {
    path.iter().map(u8::to_ascii_lowercase).collect()
}

fn run_git<I, S>(repository_root: &Path, arguments: I) -> Result<Output, BranchDeployError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    // Planning must not inherit Git variables that can select another repository or inject config.
    command.env_clear();
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    // System and user configuration are outside the selected repository and cannot affect planning.
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", GIT_CONFIG_DISABLED_PATH)
        // `git status` must not refresh the index or create an optional lock while inspecting dirty state.
        .env("GIT_OPTIONAL_LOCKS", GIT_OPTIONAL_LOCKS_DISABLED)
        .env("GIT_PAGER", GIT_SAFE_PAGER)
        // Planner commands must not invoke repository-configured monitors, hooks, pagers, or external diffs.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.pager=cat",
            "-c",
            "diff.external=",
            "-c",
            "submodule.recurse=false",
        ])
        .arg("-C")
        .arg(repository_root)
        .args(arguments);
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BranchDeployError::InvalidArgs("`git` was not found on PATH".to_string())
        } else {
            BranchDeployError::Other(anyhow::Error::from(error).context("spawning git"))
        }
    })?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        format!("git command failed with exit {}", output.status)
    } else {
        stderr
    };
    Err(BranchDeployError::InvalidArgs(detail))
}

fn output_text(output: &Output, command: &str) -> Result<String, BranchDeployError> {
    String::from_utf8(output.stdout.clone())
        .map(|text| text.trim().to_string())
        .map_err(|_| {
            BranchDeployError::Other(anyhow::anyhow!("{command} returned non-UTF-8 output"))
        })
}

fn output_path(output: &Output, command: &str) -> Result<String, BranchDeployError> {
    output_text(output, command).and_then(|path| {
        if path.is_empty() {
            Err(BranchDeployError::Other(anyhow::anyhow!(
                "{command} returned an empty path"
            )))
        } else {
            Ok(path)
        }
    })
}
