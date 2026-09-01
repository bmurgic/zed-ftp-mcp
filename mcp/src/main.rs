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
    /// Plan a committed Git range from an explicit worktree.
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
        } => deploy_branch_command(branch_deploy::DeployBranchRequest {
            profile,
            repo_root,
            base_ref,
            head_ref,
            verify,
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
    let manifest = branch_deploy::deploy_branch(&request, profile)?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    write_branch_manifest(&mut output, &manifest)?;
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

fn branch_execution_exit(success: bool) -> Result<()> {
    if success {
        Ok(())
    } else {
        anyhow::bail!("branch deployment completed unsuccessfully")
    }
}

#[cfg(test)]
mod tests {
    use super::{branch_execution_exit, Cli, Cmd};
    use clap::Parser;

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
    }

    #[test]
    fn deploy_branch_execution_contract_cli_exits_nonzero_for_unsuccessful_manifest() {
        assert!(branch_execution_exit(true).is_ok());
        assert!(branch_execution_exit(false).is_err());
    }
}
