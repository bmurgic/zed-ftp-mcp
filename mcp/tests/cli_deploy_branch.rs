//! Runs the built `zed-ftp-mcp` binary. The profile points at a closed local port and has no
//! stored password, so the run never reaches a server.

use std::path::Path;
use std::process::Command;

fn git(repo: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=cli",
            "-c",
            "user.email=cli@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(arguments)
        .status()
        .expect("git should run");
    assert!(status.success(), "git {arguments:?} failed");
}

#[test]
fn branch_range_04_cli_forwards_merge_mode_to_the_deployment() {
    let root = tempfile::TempDir::new().expect("temp directory should be created");
    let repo = root.path().join("repo");
    // The binary reads `dirs::config_dir()`: `$HOME/Library/Application Support` on macOS,
    // `$XDG_CONFIG_HOME` on Linux.
    let config = if cfg!(target_os = "macos") {
        root.path().join("Library").join("Application Support")
    } else {
        root.path().join("config")
    };
    std::fs::create_dir_all(&repo).expect("repo directory should be created");
    std::fs::create_dir_all(config.join("zed-ftp")).expect("config directory should be created");
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), b"a\n").expect("fixture should be written");
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    std::fs::write(
        config.join("zed-ftp").join("connections.toml"),
        format!(
            "[profiles.cli-mode-probe]\nhost = \"127.0.0.1\"\nport = 1\nuser = \"nobody\"\n\
             remote_root = \"/\"\nlocal_root = {:?}\npassive = true\n",
            repo.display().to_string()
        ),
    )
    .expect("config should be written");

    let output = Command::new(env!("CARGO_BIN_EXE_zed-ftp-mcp"))
        .args(["deploy-branch", "cli-mode-probe", "--repo-root"])
        .arg(&repo)
        .args(["--base", "HEAD", "--mode", "merge"])
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", &config)
        .env_remove("ZED_FTP_LOG")
        .output()
        .expect("the binary should run");

    let manifest: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be the manifest JSON");
    assert_eq!(manifest["mode"], "merge");
    assert_eq!(manifest["profile"], "cli-mode-probe");
    assert_eq!(manifest["success"], false);
    assert!(!output.status.success());
}
