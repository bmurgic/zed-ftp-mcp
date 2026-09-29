use super::{
    map_remote_path, BlobSource, BlockedPath, BranchDeletePlan, BranchDeployError,
    BranchDeployPlan, DeleteBranchFilesRequest, DeletePathResult, DeletePathStatus,
    DeletedPathResult, DeletedPathStatus, DeployBranchRequest, FailureRecord, PlannedUpload,
    RepositorySummary, RequestedAndResolvedRef, ResolvedRefs,
};
use crate::config::Profile;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

const GIT_OPTIONAL_LOCKS_DISABLED: &str = "0";
const GIT_SAFE_PAGER: &str = "cat";

pub(super) fn null_device_for_platform(is_windows: bool) -> &'static str {
    if is_windows {
        "NUL"
    } else {
        "/dev/null"
    }
}

#[derive(Debug)]
struct TreeEntry {
    mode: String,
    object_type: String,
    object_id: String,
    bytes: u64,
}

pub(crate) struct BatchBlobReader {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BatchBlobReader {
    pub(crate) fn new(repository_root: &Path) -> Result<Self, BranchDeployError> {
        let mut command = Command::new("git");
        configure_git_command(&mut command, repository_root);
        let mut child = command
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    BranchDeployError::InvalidArgs("`git` was not found on PATH".to_string())
                } else {
                    BranchDeployError::Other(
                        anyhow::Error::from(error).context("spawning git cat-file"),
                    )
                }
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            BranchDeployError::Other(anyhow::anyhow!("git cat-file did not provide stdin"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            BranchDeployError::Other(anyhow::anyhow!("git cat-file did not provide stdout"))
        })?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

impl BlobSource for BatchBlobReader {
    fn read_blob(&mut self, object_id: &str) -> Result<Vec<u8>, BranchDeployError> {
        writeln!(self.stdin, "{object_id}")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| {
                BranchDeployError::Other(
                    anyhow::Error::from(error).context("writing git cat-file request"),
                )
            })?;

        let mut header = Vec::new();
        self.stdout
            .read_until(b'\n', &mut header)
            .map_err(|error| {
                BranchDeployError::Other(
                    anyhow::Error::from(error).context("reading git cat-file header"),
                )
            })?;
        let header = std::str::from_utf8(header.strip_suffix(b"\n").ok_or_else(|| {
            BranchDeployError::Other(anyhow::anyhow!(
                "git cat-file returned an unterminated header"
            ))
        })?)
        .map_err(|_| {
            BranchDeployError::Other(anyhow::anyhow!("git cat-file returned a non-UTF-8 header"))
        })?;
        let mut fields = header.split_whitespace();
        let returned_id = fields.next();
        let object_type = fields.next();
        let size = fields.next();
        if returned_id != Some(object_id) || object_type != Some("blob") || fields.next().is_some()
        {
            return Err(BranchDeployError::Other(anyhow::anyhow!(
                "git cat-file returned an unexpected object header '{header}'"
            )));
        }
        let size = size
            .ok_or_else(|| {
                BranchDeployError::Other(anyhow::anyhow!(
                    "git cat-file returned a missing object size"
                ))
            })?
            .parse::<usize>()
            .map_err(|_| {
                BranchDeployError::Other(anyhow::anyhow!(
                    "git cat-file returned an invalid object size"
                ))
            })?;
        let mut bytes = vec![0; size];
        self.stdout.read_exact(&mut bytes).map_err(|error| {
            BranchDeployError::Other(anyhow::Error::from(error).context("reading git blob bytes"))
        })?;
        let mut trailer = [0; 1];
        self.stdout.read_exact(&mut trailer).map_err(|error| {
            BranchDeployError::Other(anyhow::Error::from(error).context("reading git blob trailer"))
        })?;
        if trailer != *b"\n" {
            return Err(BranchDeployError::Other(anyhow::anyhow!(
                "git cat-file returned an unexpected blob trailer"
            )));
        }
        Ok(bytes)
    }
}

pub(super) fn plan_branch(
    request: &DeployBranchRequest,
    profile: &Profile,
) -> Result<BranchDeployPlan, BranchDeployError> {
    let repository_root = validate_repository_root(&request.repo_root)?;
    reject_partial_or_promisor_repository(&repository_root)?;
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
        .filter_map(|(path, entry)| is_regular_blob(entry).then_some(path.clone()))
        .collect();
    let mut uploads = Vec::new();
    let mut deleted = Vec::new();
    let mut failures = Vec::new();

    for path in &touched_paths {
        match head_tree.get(path) {
            Some(entry) if is_regular_blob(entry) => {
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
                format!(
                    "head entry is not a deployable regular blob: {} {}",
                    entry.mode, entry.object_type
                ),
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
        mode: request.mode,
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

pub(super) fn plan_deletion(
    request: &DeleteBranchFilesRequest,
    profile: &Profile,
) -> Result<BranchDeletePlan, BranchDeployError> {
    let repository_root = validate_repository_root(&request.repo_root)?;
    reject_partial_or_promisor_repository(&repository_root)?;
    let mut plan = deletion_plan_shell(request, &repository_root);

    let base_commit = resolve_pinned_commit(&repository_root, &request.base_commit)
        .map_err(|reason| {
            plan.blocked.push(BlockedPath {
                git_path: None,
                reason,
            });
        })
        .ok();
    let head_commit = resolve_pinned_commit(&repository_root, &request.head_commit)
        .map_err(|reason| {
            plan.blocked.push(BlockedPath {
                git_path: None,
                reason,
            });
        })
        .ok();
    if !plan.blocked.is_empty() {
        plan.blocked
            .sort_by(|left, right| left.reason.cmp(&right.reason));
        return Ok(plan);
    }
    let base_commit = base_commit.expect("successful pinned commit should be present");
    let head_commit = head_commit.expect("successful pinned commit should be present");
    plan.base_commit = base_commit.clone();
    plan.head_commit = head_commit.clone();

    if request.reason.trim().is_empty() {
        plan.blocked.push(BlockedPath {
            git_path: None,
            reason: "deletion reason must not be empty".to_string(),
        });
    }
    if request.paths.is_empty() {
        plan.blocked.push(BlockedPath {
            git_path: None,
            reason: "at least one deletion path is required".to_string(),
        });
    }

    let commits = range_commits(&repository_root, &base_commit, &head_commit)?;
    let touched = touched_paths(&repository_root, &commits)?;
    let head_tree = head_tree(&repository_root, &head_commit)?;
    let deleted: BTreeSet<Vec<u8>> = touched
        .into_iter()
        .filter(|path| !head_tree.contains_key(path))
        .collect();

    let mut requested = BTreeSet::new();
    let mut checked_paths = Vec::new();
    let mut candidates = Vec::new();
    for path in &request.paths {
        if !requested.insert(path.clone()) {
            plan.blocked.push(BlockedPath {
                git_path: Some(path.clone()),
                reason: "duplicate deletion path".to_string(),
            });
            continue;
        }
        if !path.is_ascii() {
            plan.blocked.push(BlockedPath {
                git_path: Some(path.clone()),
                reason: "deletion paths must contain ASCII characters only".to_string(),
            });
            continue;
        }
        let (git_path, remote_path) = match map_remote_path(&profile.remote_root, path.as_bytes()) {
            Ok(path) => path,
            Err(error) => {
                plan.blocked.push(BlockedPath {
                    git_path: Some(path.clone()),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        checked_paths.push(git_path.clone());
        if !deleted.contains(path.as_bytes()) {
            plan.blocked.push(BlockedPath {
                git_path: Some(path.clone()),
                reason: "path is not deleted in the pinned commit range".to_string(),
            });
            continue;
        }
        candidates.push(DeletePathResult {
            git_path,
            remote_path,
            status: DeletePathStatus::Planned,
        });
    }

    let mut folded: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    for path in &checked_paths {
        folded
            .entry(ascii_fold(path.as_bytes()))
            .or_default()
            .push(path.clone());
    }
    for (path, entry) in &head_tree {
        if entry.object_type == "blob" {
            folded
                .entry(ascii_fold(path))
                .or_default()
                .push(String::from_utf8_lossy(path).into_owned());
        }
    }
    for path in &checked_paths {
        let collides = folded
            .get(&ascii_fold(path.as_bytes()))
            .is_some_and(|paths| paths.len() > 1);
        if collides {
            plan.blocked.push(BlockedPath {
                git_path: Some(path.clone()),
                reason: "path ASCII-case-collides with another requested or surviving path"
                    .to_string(),
            });
        }
    }

    plan.blocked.sort_by(|left, right| {
        left.git_path
            .cmp(&right.git_path)
            .then(left.reason.cmp(&right.reason))
    });
    if plan.blocked.is_empty() {
        candidates.sort_by(|left, right| left.git_path.cmp(&right.git_path));
        plan.paths = candidates;
    }
    Ok(plan)
}

fn deletion_plan_shell(
    request: &DeleteBranchFilesRequest,
    repository_root: &Path,
) -> BranchDeletePlan {
    BranchDeletePlan {
        profile: request.profile.clone(),
        repository_root: repository_root.display().to_string(),
        base_commit: request.base_commit.clone(),
        head_commit: request.head_commit.clone(),
        reason: request.reason.clone(),
        dry_run: request.dry_run,
        paths: Vec::new(),
        blocked: Vec::new(),
        failures: Vec::new(),
    }
}

fn resolve_pinned_commit(repository_root: &Path, commit: &str) -> Result<String, String> {
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(
            "commit identifier must be a 40-character lowercase hexadecimal ID".to_string(),
        );
    }
    match resolve_commit(repository_root, commit) {
        Ok(resolved) if resolved == commit => Ok(resolved),
        Ok(_) => Err("commit identifier did not resolve to the supplied canonical ID".to_string()),
        Err(error) => Err(format!("commit identifier could not be resolved: {error}")),
    }
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
                mode: std::str::from_utf8(fields[0])
                    .map_err(|_| {
                        BranchDeployError::Other(anyhow::anyhow!(
                            "git ls-tree returned a non-UTF-8 mode"
                        ))
                    })?
                    .to_string(),
                object_type: object_type.to_string(),
                object_id: object_id.to_string(),
                bytes,
            },
        );
    }
    Ok(tree)
}

fn is_regular_blob(entry: &TreeEntry) -> bool {
    entry.object_type == "blob" && matches!(entry.mode.as_str(), "100644" | "100755")
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

fn reject_partial_or_promisor_repository(repository_root: &Path) -> Result<(), BranchDeployError> {
    let output = run_git(repository_root, ["config", "--null", "--list"])?;
    for (key, _) in parse_repository_git_config(&output.stdout)? {
        let normalized_key = key.to_ascii_lowercase();
        if normalized_key == "extensions.partialclone"
            || normalized_key.starts_with("remote.")
                && normalized_key.ends_with(".promisor")
                && git_config_boolean_is_true(repository_root, &key)?
        {
            return Err(BranchDeployError::InvalidArgs(
                "partial/promisor repositories are not supported for branch deployment planning"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn git_config_boolean_is_true(
    repository_root: &Path,
    key: &str,
) -> Result<bool, BranchDeployError> {
    let output = run_git(repository_root, ["config", "--type=bool", "--get-all", key])?;
    let values = output_text(&output, "git config --type=bool --get-all")?;
    if values.is_empty() {
        return Err(BranchDeployError::Other(anyhow::anyhow!(
            "git config returned no canonical boolean values for '{key}'"
        )));
    }

    let mut has_true_value = false;
    for value in values.lines() {
        match value {
            "true" => has_true_value = true,
            "false" => {}
            _ => {
                return Err(BranchDeployError::Other(anyhow::anyhow!(
                    "git config returned invalid canonical boolean value for '{key}'"
                )))
            }
        }
    }
    Ok(has_true_value)
}

fn parse_repository_git_config(config: &[u8]) -> Result<Vec<(String, String)>, BranchDeployError> {
    let mut entries = Vec::new();
    for record in config
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
    {
        let Some(separator) = record.iter().position(|byte| *byte == b'\n') else {
            return Err(BranchDeployError::Other(anyhow::anyhow!(
                "git config returned malformed NUL-delimited output"
            )));
        };
        let key = std::str::from_utf8(&record[..separator]).map_err(|_| {
            BranchDeployError::Other(anyhow::anyhow!("git config returned a non-UTF-8 key"))
        })?;
        let value = std::str::from_utf8(&record[separator + 1..]).map_err(|_| {
            BranchDeployError::Other(anyhow::anyhow!("git config returned a non-UTF-8 value"))
        })?;
        entries.push((key.to_string(), value.to_string()));
    }
    Ok(entries)
}

fn run_git<I, S>(repository_root: &Path, arguments: I) -> Result<Output, BranchDeployError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    configure_git_command(&mut command, repository_root);
    let output = command.args(arguments).output().map_err(|error| {
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

pub(super) fn configure_git_command(command: &mut Command, repository_root: &Path) {
    let null_device = null_device_for_platform(cfg!(windows));
    let hooks_path = format!("core.hooksPath={null_device}");
    // Planning must not inherit Git variables that can select another repository or inject config.
    command.env_clear();
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    // System and user configuration are outside the selected repository and cannot affect planning.
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device)
        // Replacement refs can make recorded commit IDs disagree with planned trees and blobs.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        // This is defense in depth only; partial and promisor repositories are rejected before object inspection.
        .env("GIT_NO_LAZY_FETCH", "1")
        // `git status` must not refresh the index or create an optional lock while inspecting dirty state.
        .env("GIT_OPTIONAL_LOCKS", GIT_OPTIONAL_LOCKS_DISABLED)
        .env("GIT_PAGER", GIT_SAFE_PAGER)
        // Planner commands must not invoke repository-configured monitors, hooks, pagers, or external diffs.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            hooks_path.as_str(),
            "-c",
            "core.pager=cat",
            "-c",
            "diff.external=",
            "-c",
            "submodule.recurse=false",
        ])
        .arg("-C")
        .arg(repository_root);
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
