//! MCP tool definitions exposed to Zed's agent.
//!
//! Each tool is a thin async wrapper that loads config, then runs blocking
//! FTP work on a tokio blocking thread (suppaftp is sync).

use crate::{branch_deploy, config::Config, deploy, drift::DriftCheck};
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
    /// Git ref (for example the branch or commit the server was last deployed from). When set,
    /// the file's server copy is downloaded first, and nothing is uploaded if it differs from
    /// both the copy at this ref and the copy being uploaded.
    #[serde(default)]
    pub expect_ref: Option<String>,
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
    /// Git ref (for example the branch or commit the server was last deployed from). When set,
    /// each file's server copy is downloaded first, and the whole run is refused if any file
    /// differs from both the copy at this ref and the copy being uploaded. A dry run with this
    /// set connects to the server to check, and never writes.
    #[serde(default)]
    pub expect_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployCommitsArgs {
    pub profile: String,
    /// One or more commit SHAs whose changed files should be uploaded.
    pub commits: Vec<String>,
    /// If true, list what *would* be uploaded without sending anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Git ref (for example the branch or commit the server was last deployed from). When set,
    /// each file's server copy is downloaded first, and the whole run is refused if any file
    /// differs from both the copy at this ref and the copy being uploaded. A dry run with this
    /// set connects to the server to check, and never writes.
    #[serde(default)]
    pub expect_ref: Option<String>,
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
    /// Preview instead of deploying. In overwrite mode, dry_run=true avoids credential and FTP access. In merge mode, dry_run=true connects to the server to preview the merge and never writes.
    #[serde(default)]
    pub dry_run: bool,
    /// `overwrite` (default) uploads head blobs as they are. `merge` three-way merges each file with its server copy.
    #[serde(default)]
    pub mode: branch_deploy::DeployMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteBranchFilesArgs {
    pub profile: String,
    /// Absolute path to the exact Git worktree root.
    pub repo_root: String,
    /// Full, canonical base commit ID.
    pub base_commit: String,
    /// Full, canonical head commit ID.
    pub head_commit: String,
    /// Exact Git paths approved for deletion.
    pub paths: Vec<String>,
    /// Why the requested remote files must be deleted.
    pub reason: String,
    /// Return the preflight-authorized paths without credentials or FTP.
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
    /// The bytes uploaded. 0 when a drift check refused the upload.
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: u64,
    /// Present only when `expect_ref` was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_check: Option<DriftCheck>,
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
    #[tool(description = "List FTP connection profiles configured in \
            ~/.config/zed-ftp/connections.toml, including whether each \
            profile has a password stored in the OS keychain.")]
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

    #[tool(description = "Test an FTP connection profile by connecting, \
            authenticating, and reporting the server's working directory.")]
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
            host: cfg
                .profile(&profile)
                .map(|p| p.host.clone())
                .unwrap_or_default(),
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
            None => {
                if remote_root.is_empty() {
                    None
                } else {
                    Some(remote_root)
                }
            }
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

    #[tool(description = "Upload a single local file to the FTP server. \
            Parent directories are created if missing. \
            Set before_changes=true to upload the last-committed (git HEAD) \
            version instead of the current working-tree content. \
            Set expect_ref (e.g. the branch or commit the server was last \
            deployed from) when the user asks to deploy without clobbering \
            server-side changes.")]
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
            expect_ref,
        } = args;
        let pname = profile.clone();
        let full_remote =
            deploy::upload_file_remote_path(&p.remote_root, &remote_path).map_err(deploy_error)?;
        let local_for_blocking = local_path.clone();
        let remote_for_blocking = full_remote.clone();
        let result = tokio::task::spawn_blocking(move || {
            deploy::upload_file(
                &deploy::UploadFileRequest {
                    local_path: &local_for_blocking,
                    remote_path: &remote_for_blocking,
                    before_changes,
                    expect_ref: expect_ref.as_deref(),
                },
                || crate::ftp::FtpClient::connect(&pname, &p),
            )
        })
        .await
        .map_err(internal)?;

        upload_response_from_result(profile, local_path, full_remote, result).map(Json)
    }

    #[tool(description = "Recursively deploy a local directory to the FTP \
            server. Respects .gitignore and per-profile ignore patterns. \
            Set dry_run=true to preview the file list without uploading. \
            Set expect_ref (e.g. the branch or commit the server was last \
            deployed from) when the user asks to deploy without clobbering \
            server-side changes.")]
    async fn ftp_deploy(
        &self,
        Parameters(DeployArgs {
            profile,
            dry_run,
            expect_ref,
        }): Parameters<DeployArgs>,
    ) -> Result<Json<deploy::DeployPlan>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&profile)
            .ok_or_else(|| invalid(format!("no profile '{profile}'")))?
            .clone();
        let pname = profile.clone();
        let result = tokio::task::spawn_blocking(move || {
            deploy::deploy(&pname, &p, dry_run, expect_ref.as_deref(), || {
                crate::ftp::FtpClient::connect(&pname, &p)
            })
        })
        .await
        .map_err(internal)?;
        deploy_plan_from_result(result)
    }

    #[tool(description = "Upload only the files changed by the given commits. \
            Each commit SHA is resolved via `git diff-tree` against its \
            parent. Files deleted in those commits are skipped. Set \
            dry_run=true to preview. \
            Set expect_ref (e.g. the branch or commit the server was last \
            deployed from) when the user asks to deploy without clobbering \
            server-side changes.")]
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
            expect_ref,
        } = args;
        let result = tokio::task::spawn_blocking(move || {
            deploy::deploy_commits(
                &profile,
                &p,
                &commits,
                dry_run,
                expect_ref.as_deref(),
                || crate::ftp::FtpClient::connect(&profile, &p),
            )
        })
        .await
        .map_err(internal)?;
        deploy_plan_from_result(result)
    }

    #[tool(
        description = "Deploy a committed Git range from an explicit worktree. \
            The plan uses exact head-commit blobs and reports removed Git paths. \
            In overwrite mode, dry_run=true avoids credential and FTP access. \
            In merge mode, dry_run=true connects to the server to preview the merge \
            and never writes. \
            When the user asks to merge into a server or profile (for example \
            'merge into staging') or to preserve server-side changes, set \
            mode=\"merge\"; run with dry_run=true first to preview conflicts."
    )]
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
            mode: args.mode,
        };
        let result =
            tokio::task::spawn_blocking(move || branch_deploy::deploy_branch(&request, &profile))
                .await
                .map_err(internal)?;

        branch_deploy_manifest_from_operation(|| result)
    }

    #[tool(
        description = "Delete explicitly approved branch-removed files. Requires full pinned commits, exact paths, and a non-empty reason. This operation is separate from branch deployment."
    )]
    async fn ftp_delete_branch_files(
        &self,
        Parameters(args): Parameters<DeleteBranchFilesArgs>,
    ) -> Result<Json<branch_deploy::BranchDeleteManifest>, ErrorData> {
        let config = Config::load().map_err(internal)?;
        let profile = config
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let request = branch_deploy::DeleteBranchFilesRequest {
            profile: args.profile,
            repo_root: args.repo_root,
            base_commit: args.base_commit,
            head_commit: args.head_commit,
            paths: args.paths,
            reason: args.reason,
            dry_run: args.dry_run,
        };
        let result = tokio::task::spawn_blocking(move || {
            branch_deploy::delete_branch_files(&request, &profile)
        })
        .await
        .map_err(internal)?;

        deletion_manifest_from_operation(|| result)
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
        let DownloadFileArgs {
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
        Ok(Json(DownloadResponse {
            profile,
            remote_path: full_path,
            content,
            encoding,
            bytes,
        }))
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
        let MkdirArgs {
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
            c.mkdir_p(&full_path_blocking)?;
            c.quit();
            Ok(())
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;

        Ok(Json(MkdirResponse {
            profile,
            remote_path: full_path,
        }))
    }

    #[tool(description = "Delete a directory from the FTP server. \
            The directory must be empty. \
            remote_root is prepended automatically.")]
    async fn ftp_delete_dir(
        &self,
        Parameters(args): Parameters<DeleteDirArgs>,
    ) -> Result<Json<DeleteResponse>, ErrorData> {
        let cfg = Config::load().map_err(internal)?;
        let p = cfg
            .profile(&args.profile)
            .ok_or_else(|| invalid(format!("no profile '{}'", args.profile)))?
            .clone();
        let DeleteDirArgs {
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
            c.rmdir(&full_path_blocking)?;
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

    #[tool(description = "Delete a single file from the FTP server. \
            The profile's remote_root is prepended to remote_path, \
            matching the behavior of ftp_deploy.")]
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

fn upload_response_from_result(
    profile: String,
    local_path: String,
    remote_path: String,
    result: Result<deploy::UploadFileOutcome, deploy::DeployError>,
) -> Result<UploadResponse, ErrorData> {
    let outcome = result.map_err(deploy_error)?;
    Ok(UploadResponse {
        profile,
        local_path,
        remote_path,
        bytes: outcome.bytes,
        drift_check: outcome.drift_check,
    })
}

fn deploy_plan_from_result(
    result: Result<deploy::DeployPlan, deploy::DeployError>,
) -> Result<Json<deploy::DeployPlan>, ErrorData> {
    result.map(Json).map_err(deploy_error)
}

/// A mistake the caller can fix is `invalid_params`. Everything else is `internal_error`.
fn deploy_error(error: deploy::DeployError) -> ErrorData {
    match error {
        deploy::DeployError::InvalidArgs(message) => invalid(message),
        deploy::DeployError::Other(error) => internal(error),
    }
}

fn branch_deploy_error(error: branch_deploy::BranchDeployError) -> ErrorData {
    match error {
        branch_deploy::BranchDeployError::InvalidArgs(message) => invalid(message),
        branch_deploy::BranchDeployError::Other(error) => internal(error),
    }
}

fn branch_deploy_manifest_output(
    manifest: branch_deploy::BranchDeployManifest,
) -> Json<branch_deploy::BranchDeployManifest> {
    Json(manifest)
}

fn branch_deploy_manifest_from_operation<F>(
    operation: F,
) -> Result<Json<branch_deploy::BranchDeployManifest>, ErrorData>
where
    F: FnOnce() -> Result<branch_deploy::BranchDeployManifest, branch_deploy::BranchDeployError>,
{
    operation()
        .map(branch_deploy_manifest_output)
        .map_err(branch_deploy_error)
}

fn deletion_manifest_output(
    manifest: branch_deploy::BranchDeleteManifest,
) -> Json<branch_deploy::BranchDeleteManifest> {
    Json(manifest)
}

fn deletion_manifest_from_operation<F>(
    operation: F,
) -> Result<Json<branch_deploy::BranchDeleteManifest>, ErrorData>
where
    F: FnOnce() -> Result<branch_deploy::BranchDeleteManifest, branch_deploy::BranchDeployError>,
{
    operation()
        .map(deletion_manifest_output)
        .map_err(branch_deploy_error)
}

fn use_base64(bytes: &[u8]) -> String {
    use std::fmt::Write;
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        let _ = write!(out, "{}", TABLE[b0 >> 2] as char);
        let _ = write!(out, "{}", TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
        let _ = write!(
            out,
            "{}",
            if chunk.len() > 1 {
                TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char
            } else {
                '='
            }
        );
        let _ = write!(
            out,
            "{}",
            if chunk.len() > 2 {
                TABLE[b2 & 0x3f] as char
            } else {
                '='
            }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        branch_deploy_error, branch_deploy_manifest_from_operation, branch_deploy_manifest_output,
        deletion_manifest_from_operation, deletion_manifest_output, deploy_plan_from_result,
        upload_response_from_result, use_base64, DeleteBranchFilesArgs, DeployArgs,
        DeployBranchArgs, DeployCommitsArgs, UploadFileArgs,
    };
    use crate::branch_deploy::{
        deletion_dry_run_manifest, dry_run_manifest, BranchDeletePlan, BranchDeployError,
        BranchDeployPlan, FailureRecord, PlannedUpload,
    };
    use rmcp::model::ErrorCode;

    fn listed_tool(name: &str) -> rmcp::model::Tool {
        super::FtpServer::tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
    }

    fn deploy_branch_tool() -> rmcp::model::Tool {
        listed_tool("ftp_deploy_branch")
    }

    #[test]
    fn upload_tool_descriptions_tell_the_agent_when_to_set_expect_ref() {
        const EXPECT_REF_WORDING: &str =
            "Set expect_ref (e.g. the branch or commit the server was \
             last deployed from) when the user asks to deploy without clobbering server-side \
             changes.";

        for name in ["ftp_upload_file", "ftp_deploy", "ftp_deploy_commits"] {
            let tool = listed_tool(name);
            let description = tool.description.as_deref().unwrap_or_default();
            let expect_ref_doc = tool
                .input_schema
                .get("properties")
                .and_then(|properties| properties.get("expect_ref"))
                .and_then(|expect_ref| expect_ref.get("description"))
                .and_then(|description| description.as_str());

            assert!(
                description.contains(EXPECT_REF_WORDING),
                "{name}: {description}"
            );
            assert!(
                expect_ref_doc.is_some(),
                "{name} should document expect_ref"
            );
        }
        for name in ["ftp_deploy_branch", "ftp_delete_file"] {
            let tool = listed_tool(name);
            assert!(
                !tool
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .contains("expect_ref"),
                "{name} has no drift guard"
            );
        }
    }

    #[test]
    fn deploy_branch_tool_description_tells_the_agent_when_to_merge() {
        let tool = deploy_branch_tool();
        let description = tool.description.as_deref().unwrap_or_default();

        assert!(description.contains(
            "When the user asks to merge into a server or profile (for example 'merge into \
             staging') or to preserve server-side changes, set mode=\"merge\"; run with \
             dry_run=true first to preview conflicts."
        ));
    }

    #[test]
    fn deploy_branch_docs_say_a_merge_preview_connects_but_never_writes() {
        const PREVIEW_WORDING: &str = "In merge mode, dry_run=true connects to the server to \
             preview the merge and never writes.";
        let tool = deploy_branch_tool();
        let description = tool.description.as_deref().unwrap_or_default();
        let dry_run_doc = tool
            .input_schema
            .get("properties")
            .and_then(|properties| properties.get("dry_run"))
            .and_then(|dry_run| dry_run.get("description"))
            .and_then(|description| description.as_str())
            .expect("dry_run should carry a description");

        assert!(description.contains(PREVIEW_WORDING), "{description}");
        assert!(dry_run_doc.contains(PREVIEW_WORDING), "{dry_run_doc}");
        for text in [description, dry_run_doc] {
            assert!(
                text.contains("In overwrite mode, dry_run=true avoids credential and FTP access."),
                "{text}"
            );
        }
    }

    #[test]
    fn deploy_tools_take_an_optional_expect_ref_that_defaults_to_none() {
        let without: DeployArgs = serde_json::from_value(serde_json::json!({"profile": "qa"}))
            .expect("ftp_deploy arguments without expect_ref should deserialize");
        let with: DeployArgs = serde_json::from_value(
            serde_json::json!({"profile": "qa", "dry_run": true, "expect_ref": "base"}),
        )
        .expect("ftp_deploy arguments with expect_ref should deserialize");
        let commits_without: DeployCommitsArgs =
            serde_json::from_value(serde_json::json!({"profile": "qa", "commits": ["abc"]}))
                .expect("ftp_deploy_commits arguments without expect_ref should deserialize");
        let commits_with: DeployCommitsArgs = serde_json::from_value(
            serde_json::json!({"profile": "qa", "commits": ["abc"], "expect_ref": "base"}),
        )
        .expect("ftp_deploy_commits arguments with expect_ref should deserialize");

        let upload_without: UploadFileArgs = serde_json::from_value(
            serde_json::json!({"profile": "qa", "local_path": "a", "remote_path": "b"}),
        )
        .expect("ftp_upload_file arguments without expect_ref should deserialize");
        let upload_with: UploadFileArgs = serde_json::from_value(serde_json::json!({
            "profile": "qa", "local_path": "a", "remote_path": "b", "expect_ref": "HEAD"
        }))
        .expect("ftp_upload_file arguments with expect_ref should deserialize");

        assert_eq!(upload_without.expect_ref, None);
        assert_eq!(upload_with.expect_ref.as_deref(), Some("HEAD"));
        assert_eq!(without.expect_ref, None);
        assert_eq!(with.expect_ref.as_deref(), Some("base"));
        assert_eq!(commits_without.expect_ref, None);
        assert_eq!(commits_with.expect_ref.as_deref(), Some("base"));
    }

    #[test]
    fn deploy_errors_map_invalid_args_to_invalid_params_and_the_rest_to_internal() {
        let invalid = deploy_plan_from_result(Err(crate::deploy::DeployError::InvalidArgs(
            "expect_ref 'nope' does not resolve to a commit".to_string(),
        )))
        .err()
        .expect("invalid arguments must be an error");
        let internal = deploy_plan_from_result(Err(crate::deploy::DeployError::Other(
            anyhow::anyhow!("downloading /site/a.txt for the drift check failed"),
        )))
        .err()
        .expect("an internal failure must be an error");

        assert_eq!(invalid.code, ErrorCode::INVALID_PARAMS);
        assert!(invalid.message.contains("does not resolve"), "{invalid:?}");
        assert_eq!(internal.code, ErrorCode::INTERNAL_ERROR);
        assert!(internal.message.contains("/site/a.txt"), "{internal:?}");
    }

    #[test]
    fn upload_response_carries_the_drift_check_and_maps_errors() {
        let refused = crate::deploy::UploadFileOutcome {
            bytes: 0,
            drift_check: Some(crate::drift::DriftCheck {
                expect_ref: "HEAD".to_string(),
                resolved_commit: "a".repeat(40),
                checked: 1,
                refused: true,
                drifted: vec![crate::drift::DriftedFile {
                    remote_path: "/home/test/Mails.php".to_string(),
                    reason: crate::drift::DriftReason::ContentDiffers,
                }],
            }),
        };

        let response = upload_response_from_result(
            "qa".to_string(),
            "/site/Mails.php".to_string(),
            "/home/test/Mails.php".to_string(),
            Ok(refused),
        )
        .expect("a refusal is a response");
        let value = serde_json::to_value(response).expect("response should serialize");

        assert_eq!(value["bytes"], serde_json::json!(0));
        assert_eq!(value["remote_path"], "/home/test/Mails.php");
        assert_eq!(value["drift_check"]["refused"], serde_json::json!(true));
        assert_eq!(
            value["drift_check"]["drifted"][0]["reason"],
            "content_differs"
        );

        let invalid = upload_response_from_result(
            "qa".to_string(),
            "a".to_string(),
            "b".to_string(),
            Err(crate::deploy::DeployError::InvalidArgs(
                "expect_ref needs a Git worktree".to_string(),
            )),
        )
        .expect_err("invalid arguments must be an error");
        assert_eq!(invalid.code, ErrorCode::INVALID_PARAMS);
    }

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
        assert_eq!(args.mode, crate::branch_deploy::DeployMode::Overwrite);
        assert_eq!(
            branch_deploy_error(BranchDeployError::InvalidArgs("bad repository".to_string())).code,
            ErrorCode::INVALID_PARAMS
        );
    }

    #[test]
    fn branch_range_05_mcp_rejects_an_unknown_mode() {
        for bad_mode in ["rebase", "MERGE"] {
            let result = serde_json::from_value::<DeployBranchArgs>(serde_json::json!({
                "profile": "staging",
                "repo_root": "/repo",
                "base_ref": "origin/dev",
                "mode": bad_mode
            }));

            assert!(result.is_err(), "mode {bad_mode} must not deserialize");
        }
        let merge: DeployBranchArgs = serde_json::from_value(serde_json::json!({
            "profile": "staging",
            "repo_root": "/repo",
            "base_ref": "origin/dev",
            "mode": "merge"
        }))
        .expect("merge mode should deserialize");
        assert_eq!(merge.mode, crate::branch_deploy::DeployMode::Merge);
    }

    #[test]
    fn deletion_contract_mcp_requires_explicit_pinned_values() {
        let args: DeleteBranchFilesArgs = serde_json::from_value(serde_json::json!({
            "profile": "staging",
            "repo_root": "/repo",
            "base_commit": "aabbccddeeff00112233445566778899aabbccdd",
            "head_commit": "11223344556677889900aabbccddeeff11223344",
            "paths": ["gone.txt"],
            "reason": "approved cleanup",
            "dry_run": true
        }))
        .expect("MCP deletion arguments should deserialize");

        assert_eq!(args.paths, vec!["gone.txt"]);
        assert_eq!(args.reason, "approved cleanup");
        assert!(args.dry_run);

        let manifest = crate::branch_deploy::deletion_dry_run_manifest(
            crate::branch_deploy::BranchDeletePlan {
                profile: args.profile,
                repository_root: args.repo_root,
                base_commit: args.base_commit,
                head_commit: args.head_commit,
                reason: args.reason,
                dry_run: true,
                paths: Vec::new(),
                blocked: vec![crate::branch_deploy::BlockedPath {
                    git_path: Some("gone.txt".to_string()),
                    reason: "path is not deleted in the pinned commit range".to_string(),
                }],
                failures: Vec::new(),
            },
        );
        assert!(!deletion_manifest_output(manifest).0.success);
    }

    #[test]
    fn deploy_branch_execution_contract_mcp_returns_unsuccessful_manifest_as_data() {
        let mut manifest = dry_run_manifest(BranchDeployPlan::empty("staging", "/repo"), true);
        manifest.success = false;

        let response = branch_deploy_manifest_output(manifest);

        assert!(!response.0.success);
    }

    #[test]
    fn deploy_branch_handler_mapping_returns_unsuccessful_manifest_as_data() {
        let mut plan = BranchDeployPlan::empty("staging", "/repo");
        plan.failures.push(FailureRecord {
            stage: "upload".to_string(),
            git_path: Some("app.bin".to_string()),
            error: "FTP write failed".to_string(),
        });
        let manifest = dry_run_manifest(plan, true);
        let expected = serde_json::to_value(&manifest).expect("manifest should serialize");

        let response = branch_deploy_manifest_from_operation(|| Ok(manifest))
            .expect("unsuccessful deployment manifest should remain MCP data");

        assert!(!response.0.success);
        assert_eq!(
            serde_json::to_value(response.0).expect("MCP response should serialize"),
            expected
        );
    }

    #[test]
    fn deletion_handler_mapping_returns_unsuccessful_manifest_as_data() {
        let manifest = deletion_dry_run_manifest(BranchDeletePlan {
            profile: "staging".to_string(),
            repository_root: "/repo".to_string(),
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            reason: "approved cleanup".to_string(),
            dry_run: false,
            paths: Vec::new(),
            blocked: vec![crate::branch_deploy::BlockedPath {
                git_path: None,
                reason: "preflight failed".to_string(),
            }],
            failures: Vec::new(),
        });
        let expected = serde_json::to_value(&manifest).expect("manifest should serialize");

        let response = deletion_manifest_from_operation(|| Ok(manifest))
            .expect("unsuccessful deletion manifest should remain MCP data");

        assert!(!response.0.success);
        assert_eq!(
            serde_json::to_value(response.0).expect("MCP response should serialize"),
            expected
        );
    }

    #[test]
    fn base64_encoder_handles_complete_and_padded_chunks() {
        assert_eq!(use_base64(&[0xff, 0xee, 0xdd, 0xcc, 0xbb]), "/+7dzLs=");
    }

    #[test]
    fn deploy_branch_contract_mcp_omits_unmeasured_remote_bytes() {
        let mut plan = BranchDeployPlan::empty("staging", "/repo");
        plan.uploads.push(PlannedUpload {
            git_path: "app.bin".to_string(),
            remote_path: "/remote/app.bin".to_string(),
            object_id: "object".to_string(),
            bytes: 4,
            base_object_id: None,
        });
        let manifest = dry_run_manifest(plan, true);
        let response = branch_deploy_manifest_output(manifest);
        let value = serde_json::to_value(response.0).expect("MCP manifest should serialize");

        assert!(value.pointer("/uploads/0/remote_bytes_read").is_none());
    }

    #[test]
    fn deploy_branch_contract_mcp_serializes_measured_remote_bytes() {
        let mut plan = BranchDeployPlan::empty("staging", "/repo");
        plan.uploads.push(PlannedUpload {
            git_path: "app.bin".to_string(),
            remote_path: "/remote/app.bin".to_string(),
            object_id: "object".to_string(),
            bytes: 4,
            base_object_id: None,
        });
        let mut manifest = dry_run_manifest(plan, true);
        manifest.uploads[0].remote_bytes_read = Some(11);

        let value = serde_json::to_value(branch_deploy_manifest_output(manifest).0)
            .expect("MCP manifest should serialize");

        assert_eq!(
            value.pointer("/uploads/0/remote_bytes_read"),
            Some(&serde_json::json!(11))
        );
    }
}
