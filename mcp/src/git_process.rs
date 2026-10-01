//! Git process plumbing shared by the branch planner, the merge tool, the drift guard, and the
//! deploy tools: how a `git` child process starts, what a spawn failure means, and which tree
//! entries count as regular files.
//!
//! Two ways to start Git exist on purpose. `configure_git_command` isolates the branch planner
//! and the merge tool from the caller's environment and configuration. `run_git` starts a plain
//! `git -C <directory>` for the drift guard and the older deploy tools, which have always
//! inherited the caller's environment.

use std::env;
use std::path::Path;
use std::process::{Command, Output};

const GIT_OPTIONAL_LOCKS_DISABLED: &str = "0";
const GIT_SAFE_PAGER: &str = "cat";

/// Why a `git` child process could not start.
#[derive(thiserror::Error, Debug)]
pub(crate) enum GitSpawnError {
    /// The caller's setup problem, so every tool reports it as invalid arguments.
    #[error("`git` was not found on PATH; install git or add it to your PATH")]
    NotFound,
    #[error(transparent)]
    Other(anyhow::Error),
}

/// A missing `git` executable is `NotFound`. Any other spawn error keeps `action` as context.
pub(crate) fn git_spawn_error(error: std::io::Error, action: &str) -> GitSpawnError {
    if error.kind() == std::io::ErrorKind::NotFound {
        GitSpawnError::NotFound
    } else {
        GitSpawnError::Other(anyhow::Error::from(error).context(action.to_string()))
    }
}

/// Variables that would point git at a repository, index, or object store other than
/// `directory`. `run_git` removes them from the child environment.
const REPOSITORY_OVERRIDE_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

fn git_command(directory: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory).args(arguments);
    for variable in REPOSITORY_OVERRIDE_VARIABLES {
        command.env_remove(variable);
    }
    command
}

/// Runs `git -C <directory> <arguments>` and returns its output, whatever the exit status. The
/// child keeps the caller's environment minus the repository override variables.
pub(crate) fn run_git(directory: &Path, arguments: &[&str]) -> Result<Output, GitSpawnError> {
    git_command(directory, arguments)
        .output()
        .map_err(|error| git_spawn_error(error, "spawning git"))
}

/// A regular or executable file blob. Symbolic links, submodules, and trees are not files a
/// deploy can upload or compare.
pub(crate) fn is_regular_file(object_type: &str, mode: &str) -> bool {
    object_type == "blob" && matches!(mode, "100644" | "100755")
}

pub(crate) fn null_device_for_platform(is_windows: bool) -> &'static str {
    if is_windows {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Isolates a `git` command from the caller's environment and configuration, and points it at
/// `repository_root`.
pub(crate) fn configure_git_command(command: &mut Command, repository_root: &Path) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_git_removes_repository_override_variables_from_the_child_environment() {
        let command = git_command(Path::new("."), &["status"]);
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for variable in REPOSITORY_OVERRIDE_VARIABLES {
            assert!(
                removed.iter().any(|name| name == variable),
                "{variable} is not removed"
            );
        }
    }

    #[test]
    fn git_spawn_error_reports_a_missing_git_as_not_found() {
        let missing = git_spawn_error(
            std::io::Error::from(std::io::ErrorKind::NotFound),
            "spawning git merge-file",
        );
        let denied = git_spawn_error(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            "spawning git merge-file",
        );

        assert_eq!(
            missing.to_string(),
            "`git` was not found on PATH; install git or add it to your PATH"
        );
        assert!(matches!(missing, GitSpawnError::NotFound));
        assert!(
            matches!(denied, GitSpawnError::Other(error) if error.to_string() == "spawning git merge-file")
        );
    }

    #[test]
    fn planner_config_uses_platform_null_devices() {
        assert_eq!(null_device_for_platform(true), "NUL");
        assert_eq!(null_device_for_platform(false), "/dev/null");
    }
}
