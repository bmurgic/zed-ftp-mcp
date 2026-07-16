//! `zed-ftp-mcp` — MCP server exposing FTP deploy tools to Zed's agent.
//!
//! Subcommands:
//!   - `serve`              run the stdio MCP server (default; what Zed invokes)
//!   - `set-password`       store a profile's FTP password in the OS keychain
//!   - `list-profiles`      print profiles loaded from the config file

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
