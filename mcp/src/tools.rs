//! MCP tool definitions exposed to Zed's agent.
//!
//! Each tool is a thin async wrapper that loads config, then runs blocking
//! FTP work on a tokio blocking thread (suppaftp is sync).

use crate::{branch_deploy, config::Config, deploy};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct FtpServer;

impl FtpServer {
    pub fn new() -> Self {
        Self
    }
}

// ─── Tool argument types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProfileArg {
    /// Connection profile name from ~/.config/zed-ftp/connections.toml
    pub profile: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Connection profile name.
    pub profile: String,
    /// Remote path. Defaults to the server's CWD after login if omitted.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadFileArgs {
    pub profile: String,
    /// Absolute or relative path to the local file to upload.
    pub local_path: String,
    /// Server-relative remote path (parent dirs created if missing).
    pub remote_path: String,
    /// If true, upload the last-committed version (git HEAD) instead of the
    /// current working-tree content. Requires the file to be inside a git repo.
    #[serde(default)]
    pub before_changes: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DownloadFileArgs {
    pub profile: String,
    /// Remote path of the file to download (remote_root prepended automatically).
    pub remote_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MkdirArgs {
    pub profile: String,
    /// Remote path of the directory to create (remote_root prepended automatically).
    pub remote_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteDirArgs {
    pub profile: String,
    /// Remote path of the directory to delete (remote_root prepended automatically).
    /// The directory must be empty.
    pub remote_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteFileArgs {
    pub profile: String,
    /// Server-absolute path of the file to delete (remote_root is prepended automatically).
    pub remote_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployArgs {
    pub profile: String,
    /// If true, list what *would* be uploaded without sending anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployCommitsArgs {
    pub profile: String,
    /// One or more commit SHAs whose changed files should be uploaded.
    pub commits: Vec<String>,
    /// If true, list what *would* be uploaded without sending anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployBranchArgs {
    pub profile: String,
    /// Absolute path to the exact Git worktree root.
    pub repo_root: String,
    /// Base Git ref for the commit range.
    pub base_ref: String,
    /// Head Git ref for the commit range. Defaults to HEAD.
    #[serde(default = "default_head_ref")]
    pub head_ref: String,
    /// Verify each uploaded file by default.
    #[serde(default = "default_verify")]
    pub verify: bool,
    /// Return the plan without reading credentials or accessing FTP.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_head_ref() -> String {
    "HEAD".to_string()
}

fn default_verify() -> bool {
    true
}

// ─── Tool output types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfileSummary {
    pub name: String,
    pub host: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub port: u16,
    pub user: String,
    pub remote_root: String,
    pub local_root: String,
    pub tls: bool,
    pub accept_invalid_certs: bool,
    pub password_stored: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfilesResponse {
    pub config_path: String,
    pub profiles: Vec<ProfileSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TestResponse {
    pub profile: String,
    pub host: String,
    pub pwd: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListResponse {
    pub profile: String,
    pub path: Option<String>,
    pub entries: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UploadResponse {
    pub profile: String,
    pub local_path: String,
    pub remote_path: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DownloadResponse {
    pub profile: String,
    pub remote_path: String,
    /// File contents as a UTF-8 string. Binary files are base64-encoded.
    pub content: String,
    pub encoding: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MkdirResponse {
    pub profile: String,
    pub remote_path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeleteResponse {
    pub profile: String,
    pub remote_path: String,
}

// ─── Tool implementations ────────────────────────────────────────────────────

#[tool_router]
impl FtpServer {
    #[tool(
        description = "List FTP connection profiles configured in \
            ~/.config/zed-ftp/connections.toml, including whether each \
            profile has a password stored in the OS keychain."
    )]
    async fn ftp_list_profiles(&self) -> Result<Json<ProfilesResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let cfg_path = crate::config::path_hint();
        let profiles = cfg
            .profiles
            .iter()
            .map(|(name, p)| ProfileSummary {
                name: name.clone(),
                host: p.host.clone(),
                port: p.port,
                user: p.user.clone(),
                remote_root: p.remote_root.clone(),
                local_root: p.local_root.clone(),
                tls: p.tls,
                accept_invalid_certs: p.accept_invalid_certs,
                password_stored: crate::config::has_password(name).unwrap_or(false),
            })
            .collect();
        Ok(Json(ProfilesResponse {
            config_path: cfg_path,
            profiles,
        }))
    }

    #[tool(
        description = "Test an FTP connection profile by connecting, \
            authenticating, and reporting the server's working directory."
    )]
    async fn ftp_test(
        &self,
        Parameters(ProfileArg { profile }): Parameters<ProfileArg>,
    ) -> Result<Json<TestResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&profile)
            .ok_or_else(|| invalid(format!("no profile '{profile}'")))?
            .clone();
        let pname = profile.clone();
        let pwd = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            let pwd = c.pwd()?;
            c.quit();
            Ok(pwd)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(TestResponse {
            host: cfg.profile(&profile).map(|p| p.host.clone()).unwrap_or_default(),
            profile,
            pwd,
        }))
    }

    #[tool(description = "List a directory on the FTP server. \
        path is relative to the profile's remote_root (prepended automatically). \
        Omit path to list remote_root itself.")]
    async fn ftp_list(
        &self,
        Parameters(ListArgs { profile, path }): Parameters<ListArgs>,
    ) -> Result<Json<ListResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&profile)
            .ok_or_else(|| invalid(format!("no profile '{profile}'")))?
            .clone();
        let pname = profile.clone();
        let remote_root = p.remote_root.trim_end_matches('/').to_string();
        let resolved_path: Option<String> = match &path {
            Some(sub) => Some(if remote_root.is_empty() {
                sub.clone()
            } else {
                format!("{remote_root}/{}", sub.trim_start_matches('/'))
            }),
            None => if remote_root.is_empty() { None } else { Some(remote_root) },
        };
        let path_for_blocking = resolved_path.clone();
        let entries = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            let entries = c.list(path_for_blocking.as_deref())?;
            c.quit();
            Ok(entries)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(ListResponse {
            profile,
            path: resolved_path,
            entries,
        }))
    }

    #[tool(
        description = "Upload a single local file to the FTP server. \
            Parent directories are created if missing. \
            Set before_changes=true to upload the last-committed (git HEAD) \
            version instead of the current working-tree content."
    )]
    async fn ftp_upload_file(
        &self,
        Parameters(args): Parameters<UploadFileArgs>,
    ) -> Result<Json<UploadResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let UploadFileArgs {
            profile,
            local_path,
            remote_path,
            before_changes,
        } = args;
        let pname = profile.clone();
        let remote_root = p.remote_root.trim_end_matches('/').to_string();
        let full_remote = if remote_root.is_empty() {
            remote_path.clone()
        } else {
            format!("{remote_root}/{}", remote_path.trim_start_matches('/'))
        };
        let local_for_blocking = local_path.clone();
        let remote_for_blocking = full_remote.clone();
        let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let content = if before_changes {
                let local = std::path::Path::new(&local_for_blocking);
                let parent = local.parent().unwrap_or(std::path::Path::new("."));
                let root_out = std::process::Command::new("git")
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
                let git_root = std::path::PathBuf::from(
                    String::from_utf8_lossy(&root_out.stdout).trim(),
                );
                let abs = local
                    .canonicalize()
                    .map_err(|e| anyhow::anyhow!("canonicalize {local_for_blocking}: {e}"))?;
                let rel = abs.strip_prefix(&git_root).map_err(|_| {
                    anyhow::anyhow!(
                        "file not under git root {}",
                        git_root.display()
                    )
                })?;
                let rel_str = rel.to_string_lossy();
                let show_out = std::process::Command::new("git")
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
                show_out.stdout
            } else {
                std::fs::read(&local_for_blocking)
                    .map_err(|e| anyhow::anyhow!("read {local_for_blocking}: {e}"))?
            };
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            let written = c.put_bytes(&remote_for_blocking, &content)?;
            c.quit();
            Ok(written)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(UploadResponse {
            profile,
            local_path,
            remote_path: full_remote,
            bytes,
        }))
    }

    #[tool(
        description = "Recursively deploy a local directory to the FTP \
            server. Respects .gitignore and per-profile ignore patterns. \
            Set dry_run=true to preview the file list without uploading."
    )]
    async fn ftp_deploy(
        &self,
        Parameters(DeployArgs { profile, dry_run }): Parameters<DeployArgs>,
    ) -> Result<Json<deploy::DeployPlan>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&profile)
            .ok_or_else(|| invalid(format!("no profile '{profile}'")))?
            .clone();
        let pname = profile.clone();
        let plan = tokio::task::spawn_blocking(move || deploy::deploy(&pname, &p, dry_run))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        Ok(Json(plan))
    }

    #[tool(
        description = "Upload only the files changed by the given commits. \
            Each commit SHA is resolved via `git diff-tree` against its \
            parent. Files deleted in those commits are skipped. Set \
            dry_run=true to preview."
    )]
    async fn ftp_deploy_commits(
        &self,
        Parameters(args): Parameters<DeployCommitsArgs>,
    ) -> Result<Json<deploy::DeployPlan>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let DeployCommitsArgs {
            profile,
            commits,
            dry_run,
        } = args;
        let result = tokio::task::spawn_blocking(move || {
            deploy::deploy_commits(&profile, &p, &commits, dry_run)
        })
        .await
        .map_err(internal)?;

        match result {
            Ok(plan) => Ok(Json(plan)),
            Err(deploy::DeployCommitsError::InvalidArgs(m)) => Err(invalid(m)),
            Err(deploy::DeployCommitsError::Other(e)) => Err(internal(e)),
        }
    }

    #[tool(description = "Plan a committed Git range from an explicit worktree. \
            The plan uses exact head-commit blobs and reports removed Git paths. \
            Set dry_run=true to avoid credential and FTP access.")]
    async fn ftp_deploy_branch(
        &self,
        Parameters(args): Parameters<DeployBranchArgs>,
    ) -> Result<Json<branch_deploy::BranchDeployManifest>, ErrorData> {
        let config = Config::load().map_err(internal)?;
        let profile = config
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let request = branch_deploy::DeployBranchRequest {
            profile: args.profile,
            repo_root: args.repo_root,
            base_ref: args.base_ref,
            head_ref: args.head_ref,
            verify: args.verify,
            dry_run: args.dry_run,
        };
        let result =
            tokio::task::spawn_blocking(move || branch_deploy::deploy_branch(&request, &profile))
                .await
                .map_err(internal)?;

        result.map(Json).map_err(branch_deploy_error)
    }

    #[tool(
        description = "Download a file from the FTP server and return its contents. \
            UTF-8 text is returned as-is; binary files are base64-encoded. \
            remote_root is prepended automatically."
    )]
    async fn ftp_download_file(
        &self,
        Parameters(args): Parameters<DownloadFileArgs>,
    ) -> Result<Json<DownloadResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let DownloadFileArgs { profile, remote_path } = args;
        let pname = profile.clone();
        let remote_root = p.remote_root.trim_end_matches('/').to_string();
        let full_path = if remote_root.is_empty() {
            remote_path.clone()
        } else {
            format!("{remote_root}/{}", remote_path.trim_start_matches('/'))
        };
        let full_path_blocking = full_path.clone();
        let raw = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            let bytes = c.get_bytes(&full_path_blocking)?;
            c.quit();
            Ok(bytes)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        let bytes = raw.len();
        let (content, encoding) = match std::str::from_utf8(&raw) {
            Ok(s) => (s.to_string(), "utf-8".to_string()),
            Err(_) => (use_base64(&raw), "base64".to_string()),
        };
        Ok(Json(DownloadResponse { profile, remote_path: full_path, content, encoding, bytes }))
    }

    #[tool(
        description = "Create a directory (and any missing parents) on the FTP server. \
            remote_root is prepended automatically."
    )]
    async fn ftp_mkdir(
        &self,
        Parameters(args): Parameters<MkdirArgs>,
    ) -> Result<Json<MkdirResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let MkdirArgs { profile, remote_path } = args;
        let pname = profile.clone();
        let remote_root = p.remote_root.trim_end_matches('/').to_string();
        let full_path = if remote_root.is_empty() {
            remote_path.clone()
        } else {
            format!("{remote_root}/{}", remote_path.trim_start_matches('/'))
        };
        let full_path_blocking = full_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            c.mkdir_p(&full_path_blocking)?;
            c.quit();
            Ok(())
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(MkdirResponse { profile, remote_path: full_path }))
    }

    #[tool(
        description = "Delete a directory from the FTP server. \
            The directory must be empty. \
            remote_root is prepended automatically."
    )]
    async fn ftp_delete_dir(
        &self,
        Parameters(args): Parameters<DeleteDirArgs>,
    ) -> Result<Json<DeleteResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let DeleteDirArgs { profile, remote_path } = args;
        let pname = profile.clone();
        let remote_root = p.remote_root.trim_end_matches('/').to_string();
        let full_path = if remote_root.is_empty() {
            remote_path.clone()
        } else {
            format!("{remote_root}/{}", remote_path.trim_start_matches('/'))
        };
        let full_path_blocking = full_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            c.rmdir(&full_path_blocking)?;
            c.quit();
            Ok(())
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(DeleteResponse { profile, remote_path: full_path }))
    }

    #[tool(
        description = "Delete a single file from the FTP server. \
            The profile's remote_root is prepended to remote_path, \
            matching the behavior of ftp_deploy."
    )]
    async fn ftp_delete_file(
        &self,
        Parameters(args): Parameters<DeleteFileArgs>,
    ) -> Result<Json<DeleteResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let DeleteFileArgs {
            profile,
            remote_path,
        } = args;
        let pname = profile.clone();
        let remote_root = p.remote_root.trim_end_matches('/').to_string();
        let full_path = if remote_root.is_empty() {
            remote_path.clone()
        } else {
            format!("{remote_root}/{}", remote_path.trim_start_matches('/'))
        };
        let full_path_blocking = full_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut c = crate::ftp::FtpClient::connect(&pname, &p)?;
            c.delete(&full_path_blocking)?;
            c.quit();
            Ok(())
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(DeleteResponse {
            profile,
            remote_path: full_path,
        }))
    }
}

#[tool_handler]
impl ServerHandler for FtpServer {}

// ─── Error helpers ───────────────────────────────────────────────────────────

fn internal<E: std::fmt::Display>(e: E) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn invalid(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

fn branch_deploy_error(error: branch_deploy::BranchDeployError) -> ErrorData {
    match error {
        branch_deploy::BranchDeployError::InvalidArgs(message) => invalid(message),
        branch_deploy::BranchDeployError::Other(error) => internal(error),
    }
}

fn use_base64(bytes: &[u8]) -> String {
    use std::fmt::Write;
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
        let _ = write!(out, "{}", TABLE[b0 >> 2] as char);
        let _ = write!(out, "{}", TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
        let _ = write!(out, "{}", if chunk.len() > 1 { TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char } else { '=' });
        let _ = write!(out, "{}", if chunk.len() > 2 { TABLE[b2 & 0x3f] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{branch_deploy_error, DeployBranchArgs};
    use crate::branch_deploy::BranchDeployError;
    use rmcp::model::ErrorCode;

    #[test]
    fn deploy_branch_contract_mcp_defaults_and_invalid_params() {
        let args: DeployBranchArgs = serde_json::from_value(serde_json::json!({
            "profile": "staging",
            "repo_root": "/repo",
            "base_ref": "origin/dev",
            "dry_run": true
        }))
        .expect("MCP arguments should deserialize");

        assert_eq!(args.head_ref, "HEAD");
        assert!(args.verify);
        assert!(args.dry_run);
        assert_eq!(
            branch_deploy_error(BranchDeployError::InvalidArgs("bad repository".to_string())).code,
            ErrorCode::INVALID_PARAMS
        );
    }
}
