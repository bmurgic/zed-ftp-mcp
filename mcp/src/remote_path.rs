//! Server paths: mapping a repository path under the profile's remote root, rejecting paths that
//! could escape it or inject FTP commands, and splitting a server path into its directory and
//! file name.

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
#[error("{0}")]
pub struct PathFailure(String);

pub(crate) fn map_remote_path(
    remote_root: &str,
    git_path: &[u8],
) -> Result<(String, String), PathFailure> {
    let git_path = std::str::from_utf8(git_path)
        .map_err(|_| PathFailure("Git path is not valid UTF-8".to_string()))?;
    validate_relative_path(git_path, "Git path")?;
    let root = normalize_remote_root(remote_root)?;
    let remote_path = if root == "/" {
        format!("/{git_path}")
    } else {
        format!("{root}/{git_path}")
    };

    if remote_path == root || !remote_path.starts_with(&(root.clone() + "/")) && root != "/" {
        return Err(PathFailure(
            "mapped remote path is not below the configured remote root".to_string(),
        ));
    }

    Ok((git_path.to_string(), remote_path))
}

/// The full server path for a tool's `remote_path`, which is relative to the profile's remote
/// root. With no remote root, `remote_path` is used as given.
pub(crate) fn resolve_remote_path(remote_root: &str, remote_path: &str) -> String {
    let root = remote_root.trim_end_matches('/');
    if root.is_empty() {
        return remote_path.to_string();
    }
    let below_root = path_below_remote_root(root, remote_path);
    if below_root.is_empty() {
        return root.to_string();
    }
    format!("{root}/{below_root}")
}

/// The part of a tool's `remote_path` below the remote root, without a leading `/`. Agents often
/// pass the full server path they saw in a listing, so an absolute path that already starts with
/// the root loses the root instead of getting it twice. Any other leading `/` is ignored.
pub(crate) fn path_below_remote_root<'a>(remote_root: &str, remote_path: &'a str) -> &'a str {
    let root = remote_root.trim_end_matches('/');
    let below_root = match remote_path.strip_prefix(root) {
        Some(rest) if root.starts_with('/') && (rest.is_empty() || rest.starts_with('/')) => rest,
        _ => remote_path,
    };
    below_root.trim_start_matches('/')
}

fn normalize_remote_root(remote_root: &str) -> Result<String, PathFailure> {
    let trimmed = remote_root.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return Ok("/".to_string());
    }
    if !trimmed.starts_with('/') {
        return Err(PathFailure(
            "configured remote root must be absolute".to_string(),
        ));
    }

    let components = trimmed.trim_matches('/');
    validate_relative_path(components, "configured remote root")?;
    Ok(format!("/{components}"))
}

/// Rejects a path that is empty or absolute, or that has an empty, `.`, or `..` component, a
/// backslash, or a control character. Such a path could leave the remote root, or a CR or LF in
/// it would end the FTP command early and start another one. `label` names the path in the error.
pub(crate) fn validate_relative_path(path: &str, label: &str) -> Result<(), PathFailure> {
    if path.is_empty() || path.starts_with('/') {
        return Err(PathFailure(format!(
            "{label} must be a non-empty relative path"
        )));
    }

    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(PathFailure(format!("{label} has an unsafe component")));
        }
        if component.contains('\\') || component.chars().any(char::is_control) {
            return Err(PathFailure(format!("{label} has an unsafe component")));
        }
    }
    Ok(())
}

/// Splits a server path into its directory and file name. The directory is `None` for a bare
/// name, which lives in the FTP working directory, and `/` for a file directly under the root.
pub(crate) fn split_parent(remote_path: &str) -> (Option<&str>, &str) {
    match remote_path.rsplit_once('/') {
        Some(("", file_name)) => (Some("/"), file_name),
        Some((parent, file_name)) => (Some(parent), file_name),
        None => (None, remote_path),
    }
}

/// The directory an upload must create before it writes `remote_path`. `None` when the file
/// lives directly under `/` or in the working directory, which always exist.
pub(crate) fn parent_to_create(remote_path: &str) -> Option<&str> {
    split_parent(remote_path).0.filter(|parent| *parent != "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_path_maps_under_remote_root() {
        let (git_path, remote_path) = map_remote_path("/remote/root/", b"assets/app.js")
            .expect("path should map under remote root");

        assert_eq!(git_path, "assets/app.js");
        assert_eq!(remote_path, "/remote/root/assets/app.js");
    }

    #[test]
    fn planner_path_rejects_absolute_dot_backslash_and_control_components() {
        for path in [
            b"/absolute".as_slice(),
            b"./dot",
            b"dir/../escape",
            b"dir\\file",
            b"dir/\x01file",
            b"dir//file",
        ] {
            assert!(
                map_remote_path("/remote/root", path).is_err(),
                "{path:?} should fail"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn planner_path_rejects_non_utf8() {
        assert!(map_remote_path("/remote/root", b"invalid-\xff").is_err());
    }

    #[test]
    fn a_tool_path_relative_to_the_remote_root_gets_the_root_prepended() {
        assert_eq!(
            resolve_remote_path("/tmsfalcon/", "application/Foo.php"),
            "/tmsfalcon/application/Foo.php"
        );
        assert_eq!(
            resolve_remote_path("/tmsfalcon", "/application/Foo.php"),
            "/tmsfalcon/application/Foo.php"
        );
    }

    #[test]
    fn a_tool_path_that_already_starts_with_the_remote_root_is_used_as_given() {
        assert_eq!(
            resolve_remote_path("/tmsfalcon", "/tmsfalcon/application/Foo.php"),
            "/tmsfalcon/application/Foo.php"
        );
        assert_eq!(
            resolve_remote_path("/tmsfalcon/", "/tmsfalcon"),
            "/tmsfalcon"
        );
        assert_eq!(
            resolve_remote_path("/tmsfalcon", "/tmsfalcon/"),
            "/tmsfalcon"
        );
    }

    #[test]
    fn a_relative_tool_path_that_names_a_folder_like_the_root_stays_below_the_root() {
        assert_eq!(
            resolve_remote_path("/tmsfalcon", "tmsfalcon/a.txt"),
            "/tmsfalcon/tmsfalcon/a.txt"
        );
        assert_eq!(
            resolve_remote_path("/tmsfalcon", "/tmsfalcon-old/a.txt"),
            "/tmsfalcon/tmsfalcon-old/a.txt"
        );
    }

    #[test]
    fn a_tool_path_with_no_remote_root_is_used_as_given() {
        assert_eq!(resolve_remote_path("", "sub/a.txt"), "sub/a.txt");
        assert_eq!(resolve_remote_path("/", "/sub/a.txt"), "/sub/a.txt");
    }

    #[test]
    fn a_server_path_splits_into_its_directory_and_file_name() {
        assert_eq!(
            split_parent("/site/dir/a.txt"),
            (Some("/site/dir"), "a.txt")
        );
        assert_eq!(split_parent("/a.txt"), (Some("/"), "a.txt"));
        assert_eq!(split_parent("a.txt"), (None, "a.txt"));
    }

    #[test]
    fn only_a_directory_below_the_root_needs_creating() {
        assert_eq!(
            parent_to_create("/home/test/sub/a.txt"),
            Some("/home/test/sub")
        );
        assert_eq!(parent_to_create("/a.txt"), None);
        assert_eq!(parent_to_create("a.txt"), None);
    }
}
