//! `zed-ftp-mcp` — MCP server exposing FTP deploy tools to Zed's agent.
//!
//! Subcommands:
//!   - `serve`              run the stdio MCP server (default; what Zed invokes)
//!   - `set-password`       store a profile's FTP password in the OS keychain
//!   - `list-profiles`      print profiles loaded from the config file

mod branch_deploy;
mod config;
mod deploy;
mod ftp;
mod schema;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "zed-ftp-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server over stdio (default).
    Serve,
    /// Store an FTP password for a profile in the OS keychain.
    SetPassword {
        /// Profile name as defined in connections.toml.
        profile: String,
    },
    /// List configured connection profiles.
    ListProfiles,
    /// Deploy a committed Git range from an explicit worktree. Use --mode merge to merge into
    /// server-side changes instead of overwriting them.
    DeployBranch {
        /// Profile name as defined in connections.toml.
        profile: String,
        /// Absolute path to the exact Git worktree root.
        #[arg(long)]
        repo_root: String,
        /// Base Git ref for the commit range.
        #[arg(long = "base")]
        base_ref: String,
        /// Head Git ref for the commit range.
        #[arg(long = "head", default_value = "HEAD")]
        head_ref: String,
        /// Disable post-upload byte verification.
        #[arg(
            long = "no-verify",
            action = clap::ArgAction::SetFalse,
            default_value_t = true
        )]
        verify: bool,
        /// Return the deployment plan without accessing FTP or credentials.
        #[arg(long)]
        dry_run: bool,
        /// Deployment mode. overwrite uploads head blobs as they are. merge three-way merges
        /// each file with its server copy, and uploads nothing if any file conflicts.
        #[arg(long, value_enum, default_value_t = branch_deploy::DeployMode::Overwrite)]
        mode: branch_deploy::DeployMode,
    },
    /// Delete exact branch-removed files after explicit approval.
    DeleteBranchFiles {
        /// Profile name as defined in connections.toml.
        profile: String,
        /// Absolute path to the exact Git worktree root.
        #[arg(long)]
        repo_root: String,
        /// Full, canonical base commit ID.
        #[arg(long)]
        base_commit: String,
        /// Full, canonical head commit ID.
        #[arg(long)]
        head_commit: String,
        /// Exact Git path to delete. Repeat for each approved path.
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
        /// Why each requested remote deletion is needed.
        #[arg(long)]
        reason: String,
        /// Return the authorized deletion plan without accessing FTP or credentials.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // MCP requires a clean stdout for JSON-RPC, so logs go to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("ZED_FTP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve().await,
        Cmd::SetPassword { profile } => set_password(&profile),
        Cmd::ListProfiles => list_profiles(),
        Cmd::DeployBranch {
            profile,
            repo_root,
            base_ref,
            head_ref,
            verify,
            dry_run,
            mode,
        } => deploy_branch_command(branch_deploy::DeployBranchRequest {
            profile,
            repo_root,
            base_ref,
            head_ref,
            verify,
            dry_run,
            mode,
        }),
        Cmd::DeleteBranchFiles {
            profile,
            repo_root,
            base_commit,
            head_commit,
            paths,
            reason,
            dry_run,
        } => delete_branch_files_command(branch_deploy::DeleteBranchFilesRequest {
            profile,
            repo_root,
            base_commit,
            head_commit,
            paths,
            reason,
            dry_run,
        }),
    }
}

async fn serve() -> Result<()> {
    let server = tools::FtpServer::new();
    let service = server
        .serve(stdio())
        .await
        .context("failed to start MCP service over stdio")?;
    service.waiting().await?;
    Ok(())
}

fn set_password(profile: &str) -> Result<()> {
    // Verify the profile exists before prompting.
    let cfg = config::Config::load()?;
    cfg.profile(profile)
        .with_context(|| format!("no profile named '{profile}' in {}", config::path_hint()))?;

    eprint!("Password for FTP profile '{profile}': ");
    let pw = rpassword::read_password().context("failed to read password from stdin")?;
    if pw.is_empty() {
        anyhow::bail!("empty password rejected");
    }

    config::store_password(profile, &pw)?;
    eprintln!("Stored password for '{profile}' in OS keychain.");
    Ok(())
}

fn list_profiles() -> Result<()> {
    let cfg = config::Config::load()?;
    if cfg.profiles.is_empty() {
        eprintln!(
            "No profiles configured. Edit {} to add one.",
            config::path_hint()
        );
        return Ok(());
    }
    for (name, p) in &cfg.profiles {
        let has_pw = config::has_password(name).unwrap_or(false);
        println!(
            "{name:<20} {host}:{port:<5} user={user} pw={pw}",
            host = p.host,
            port = p.port,
            user = p.user,
            pw = if has_pw { "stored" } else { "MISSING" },
        );
    }
    Ok(())
}

fn deploy_branch_command(request: branch_deploy::DeployBranchRequest) -> Result<()> {
    let config = config::Config::load()?;
    let profile = config.profile(&request.profile).with_context(|| {
        format!(
            "no profile named '{}' in {}",
            request.profile,
            config::path_hint()
        )
    })?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    deploy_branch_command_with_operation(
        &request,
        profile,
        &mut output,
        branch_deploy::deploy_branch,
    )
}

fn deploy_branch_command_with_operation<W, F>(
    request: &branch_deploy::DeployBranchRequest,
    profile: &config::Profile,
    output: &mut W,
    operation: F,
) -> Result<()>
where
    W: std::io::Write,
    F: FnOnce(
        &branch_deploy::DeployBranchRequest,
        &config::Profile,
    ) -> Result<branch_deploy::BranchDeployManifest, branch_deploy::BranchDeployError>,
{
    let manifest = operation(request, profile)?;
    write_branch_manifest(output, &manifest)?;
    branch_execution_exit(manifest.success)
}

fn delete_branch_files_command(request: branch_deploy::DeleteBranchFilesRequest) -> Result<()> {
    let config = config::Config::load()?;
    let profile = config.profile(&request.profile).with_context(|| {
        format!(
            "no profile named '{}' in {}",
            request.profile,
            config::path_hint()
        )
    })?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    delete_branch_files_command_with_operation(
        &request,
        profile,
        &mut output,
        branch_deploy::delete_branch_files,
    )
}

fn delete_branch_files_command_with_operation<W, F>(
    request: &branch_deploy::DeleteBranchFilesRequest,
    profile: &config::Profile,
    output: &mut W,
    operation: F,
) -> Result<()>
where
    W: std::io::Write,
    F: FnOnce(
        &branch_deploy::DeleteBranchFilesRequest,
        &config::Profile,
    ) -> Result<branch_deploy::BranchDeleteManifest, branch_deploy::BranchDeployError>,
{
    let manifest = operation(request, profile)?;
    write_deletion_manifest(output, &manifest)?;
    branch_execution_exit(manifest.success)
}

fn write_branch_manifest(
    output: &mut impl std::io::Write,
    manifest: &branch_deploy::BranchDeployManifest,
) -> Result<()> {
    serde_json::to_writer_pretty(&mut *output, manifest)?;
    writeln!(output)?;
    Ok(())
}

fn write_deletion_manifest(
    output: &mut impl std::io::Write,
    manifest: &branch_deploy::BranchDeleteManifest,
) -> Result<()> {
    serde_json::to_writer_pretty(&mut *output, manifest)?;
    writeln!(output)?;
    Ok(())
}

fn branch_execution_exit(success: bool) -> Result<()> {
    if success {
        Ok(())
    } else {
        anyhow::bail!("branch deployment completed unsuccessfully")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        branch_execution_exit, delete_branch_files_command_with_operation,
        deploy_branch_command_with_operation, write_branch_manifest, Cli, Cmd,
    };
    use crate::{
        branch_deploy::{
            deletion_manifest, dry_run_manifest, BlockedPath, BranchDeletePlan, BranchDeployPlan,
            DeletePathResult, DeletePathStatus, FailureRecord, PlannedUpload,
        },
        config::Profile,
    };
    use clap::Parser;

    fn test_profile() -> Profile {
        Profile {
            host: "ftp.example.test".to_string(),
            port: 21,
            user: "zed".to_string(),
            remote_root: "/remote".to_string(),
            local_root: ".".to_string(),
            passive: true,
            tls: false,
            accept_invalid_certs: false,
            ignore: Vec::new(),
        }
    }

    fn deploy_branch_request(
        mode: crate::branch_deploy::DeployMode,
    ) -> crate::branch_deploy::DeployBranchRequest {
        crate::branch_deploy::DeployBranchRequest {
            profile: "staging".to_string(),
            repo_root: "/repo".to_string(),
            base_ref: "origin/main".to_string(),
            head_ref: "HEAD".to_string(),
            verify: true,
            dry_run: false,
            mode,
        }
    }

    fn unsuccessful_deployment_manifest() -> crate::branch_deploy::BranchDeployManifest {
        let mut plan = BranchDeployPlan::empty("staging", "/repo");
        plan.failures.push(FailureRecord {
            stage: "upload".to_string(),
            git_path: Some("app.bin".to_string()),
            error: "FTP write failed".to_string(),
        });
        dry_run_manifest(plan, true)
    }

    fn unsuccessful_deletion_manifest() -> crate::branch_deploy::BranchDeleteManifest {
        let plan = BranchDeletePlan {
            profile: "staging".to_string(),
            repository_root: "/repo".to_string(),
            base_commit: "aabbccddeeff00112233445566778899aabbccdd".to_string(),
            head_commit: "11223344556677889900aabbccddeeff11223344".to_string(),
            reason: "approved cleanup".to_string(),
            dry_run: false,
            paths: vec![DeletePathResult {
                git_path: "retired.txt".to_string(),
                remote_path: "/remote/retired.txt".to_string(),
                status: DeletePathStatus::Failed,
            }],
            blocked: vec![BlockedPath {
                git_path: None,
                reason: "operator approval recorded".to_string(),
            }],
            failures: vec![FailureRecord {
                stage: "delete".to_string(),
                git_path: Some("retired.txt".to_string()),
                error: "FTP delete failed".to_string(),
            }],
        };

        deletion_manifest(
            plan,
            vec![DeletePathResult {
                git_path: "retired.txt".to_string(),
                remote_path: "/remote/retired.txt".to_string(),
                status: DeletePathStatus::Failed,
            }],
            false,
            false,
        )
    }

    #[test]
    fn deploy_branch_contract_cli_defaults() {
        let cli = Cli::try_parse_from([
            "zed-ftp-mcp",
            "deploy-branch",
            "staging",
            "--repo-root",
            "/repo",
            "--base",
            "origin/dev",
            "--dry-run",
        ])
        .expect("CLI arguments should parse");

        let Some(Cmd::DeployBranch {
            profile,
            repo_root,
            base_ref,
            head_ref,
            verify,
            dry_run,
            mode,
        }) = cli.command
        else {
            panic!("expected deploy-branch command");
        };
        assert_eq!(profile, "staging");
        assert_eq!(repo_root, "/repo");
        assert_eq!(base_ref, "origin/dev");
        assert_eq!(head_ref, "HEAD");
        assert!(verify);
        assert!(dry_run);
        assert_eq!(mode, crate::branch_deploy::DeployMode::Overwrite);
    }

    #[test]
    fn branch_range_04_cli_accepts_merge_mode() {
        let cli = Cli::try_parse_from([
            "zed-ftp-mcp",
            "deploy-branch",
            "staging",
            "--repo-root",
            "/repo",
            "--base",
            "origin/dev",
            "--mode",
            "merge",
        ])
        .expect("CLI arguments should parse");

        let Some(Cmd::DeployBranch { mode, .. }) = cli.command else {
            panic!("expected deploy-branch command");
        };
        assert_eq!(mode, crate::branch_deploy::DeployMode::Merge);
    }

    #[test]
    fn branch_range_05_cli_rejects_an_unknown_mode() {
        for bad_mode in ["rebase", "MERGE"] {
            let error = match Cli::try_parse_from([
                "zed-ftp-mcp",
                "deploy-branch",
                "staging",
                "--repo-root",
                "/repo",
                "--base",
                "origin/dev",
                "--mode",
                bad_mode,
            ]) {
                Ok(_) => panic!("mode {bad_mode} must not parse"),
                Err(error) => error,
            };

            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            let message = error.to_string();
            assert!(message.contains("overwrite") && message.contains("merge"));
        }
    }

    #[test]
    fn deploy_branch_execution_contract_cli_exits_nonzero_for_unsuccessful_manifest() {
        assert!(branch_execution_exit(true).is_ok());
        assert!(branch_execution_exit(false).is_err());
    }

    #[test]
    fn deploy_branch_command_writes_unsuccessful_manifest_before_returning_error() {
        let request = deploy_branch_request(crate::branch_deploy::DeployMode::Overwrite);
        let expected_manifest = unsuccessful_deployment_manifest();
        let mut output = Vec::new();

        let result =
            deploy_branch_command_with_operation(&request, &test_profile(), &mut output, |_, _| {
                Ok(expected_manifest)
            });

        assert!(result.is_err());
        let manifest: serde_json::Value =
            serde_json::from_slice(&output).expect("CLI should write manifest JSON before error");
        assert_eq!(
            manifest,
            serde_json::json!({
                "success": false,
                "profile": "staging",
                "repository": { "root": "/repo", "dirty": false },
                "refs": {
                    "base": { "requested": "base", "commit": "base" },
                    "head": { "requested": "HEAD", "commit": "head" }
                },
                "merge_rule": "first_parent",
                "mode": "overwrite",
                "blocked_by_conflicts": false,
                "dry_run": true,
                "verify": true,
                "counts": {
                    "commits": 0,
                    "touched_paths": 0,
                    "planned_uploads": 0,
                    "uploaded": 0,
                    "verified": 0,
                    "deleted_reported": 0,
                    "failures": 1
                },
                "uploads": [],
                "deleted": [],
                "failures": [{
                    "stage": "upload",
                    "git_path": "app.bin",
                    "error": "FTP write failed"
                }]
            })
        );
    }

    #[test]
    fn deploy_branch_help_tells_the_reader_when_to_merge() {
        use clap::CommandFactory;
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("deploy-branch")
            .expect("deploy-branch subcommand")
            .render_long_help()
            .to_string();

        assert!(help.contains("--mode <MODE>"));
        assert!(help.contains("merge into"));
        assert!(help.contains("uploads nothing if any file conflicts"));
    }

    #[test]
    fn manifest_04_blocked_merge_is_unsuccessful_through_the_cli() {
        let request = deploy_branch_request(crate::branch_deploy::DeployMode::Merge);
        let mut blocked_manifest = unsuccessful_deployment_manifest();
        blocked_manifest.mode = crate::branch_deploy::DeployMode::Merge;
        blocked_manifest.blocked_by_conflicts = true;
        let mut output = Vec::new();

        let result =
            deploy_branch_command_with_operation(&request, &test_profile(), &mut output, |_, _| {
                Ok(blocked_manifest)
            });

        assert!(result.is_err());
        let manifest: serde_json::Value =
            serde_json::from_slice(&output).expect("CLI should write manifest JSON before error");
        assert_eq!(manifest["mode"], "merge");
        assert_eq!(manifest["blocked_by_conflicts"], true);
        assert_eq!(manifest["success"], false);
    }

    #[test]
    fn delete_branch_files_command_writes_unsuccessful_manifest_before_returning_error() {
        let request = crate::branch_deploy::DeleteBranchFilesRequest {
            profile: "staging".to_string(),
            repo_root: "/repo".to_string(),
            base_commit: "aabbccddeeff00112233445566778899aabbccdd".to_string(),
            head_commit: "11223344556677889900aabbccddeeff11223344".to_string(),
            paths: vec!["retired.txt".to_string()],
            reason: "approved cleanup".to_string(),
            dry_run: false,
        };
        let expected_manifest = unsuccessful_deletion_manifest();
        let mut output = Vec::new();

        let result = delete_branch_files_command_with_operation(
            &request,
            &test_profile(),
            &mut output,
            |_, _| Ok(expected_manifest),
        );

        assert!(result.is_err());
        let manifest: serde_json::Value =
            serde_json::from_slice(&output).expect("CLI should write manifest JSON before error");
        assert_eq!(
            manifest,
            serde_json::json!({
                "success": false,
                "profile": "staging",
                "repository_root": "/repo",
                "base_commit": "aabbccddeeff00112233445566778899aabbccdd",
                "head_commit": "11223344556677889900aabbccddeeff11223344",
                "reason": "approved cleanup",
                "dry_run": false,
                "counts": {
                    "planned": 0,
                    "deleted": 0,
                    "failed": 1,
                    "not_attempted": 0,
                    "blocked": 1
                },
                "paths": [{
                    "git_path": "retired.txt",
                    "remote_path": "/remote/retired.txt",
                    "status": "failed"
                }],
                "blocked": [{
                    "git_path": null,
                    "reason": "operator approval recorded"
                }],
                "failures": [{
                    "stage": "delete",
                    "git_path": "retired.txt",
                    "error": "FTP delete failed"
                }]
            })
        );
    }

    #[test]
    fn deletion_contract_cli_parses_all_required_pinned_values() {
        let cli = Cli::try_parse_from([
            "zed-ftp-mcp",
            "delete-branch-files",
            "staging",
            "--repo-root",
            "/repo",
            "--base-commit",
            "aabbccddeeff00112233445566778899aabbccdd",
            "--head-commit",
            "11223344556677889900aabbccddeeff11223344",
            "--path",
            "first.txt",
            "--path",
            "second.txt",
            "--reason",
            "approved cleanup",
            "--dry-run",
        ])
        .expect("CLI arguments should parse");

        let Some(Cmd::DeleteBranchFiles {
            paths,
            reason,
            dry_run,
            ..
        }) = cli.command
        else {
            panic!("expected delete-branch-files command");
        };
        assert_eq!(paths, vec!["first.txt", "second.txt"]);
        assert_eq!(reason, "approved cleanup");
        assert!(dry_run);
    }

    #[test]
    fn deploy_branch_contract_cli_omits_unmeasured_remote_bytes() {
        let mut plan = BranchDeployPlan::empty("staging", "/repo");
        plan.uploads.push(PlannedUpload {
            git_path: "app.bin".to_string(),
            remote_path: "/remote/app.bin".to_string(),
            object_id: "object".to_string(),
            bytes: 4,
            base_object_id: None,
        });
        let manifest = dry_run_manifest(plan, true);
        let mut output = Vec::new();

        write_branch_manifest(&mut output, &manifest).expect("CLI should write manifest JSON");

        let value: serde_json::Value =
            serde_json::from_slice(&output).expect("CLI manifest should be valid JSON");
        assert!(value.pointer("/uploads/0/remote_bytes_read").is_none());
    }

    #[test]
    fn deploy_branch_contract_cli_serializes_measured_remote_bytes() {
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
        let mut output = Vec::new();

        write_branch_manifest(&mut output, &manifest).expect("CLI should write manifest JSON");

        let value: serde_json::Value =
            serde_json::from_slice(&output).expect("CLI manifest should be valid JSON");
        assert_eq!(
            value.pointer("/uploads/0/remote_bytes_read"),
            Some(&serde_json::json!(11))
        );
    }
}
