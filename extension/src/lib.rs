//! Zed extension entry point.
//!
//! This is intentionally tiny: the WASM sandbox cannot open sockets, so all
//! real work happens in the `zed-ftp-mcp` binary. The extension's job is to
//! tell Zed how to spawn that binary as an MCP context server.

use zed_extension_api::{self as zed, Command, ContextServerId, Project, Result};

const MCP_BINARY: &str = "zed-ftp-mcp";

struct ZedFtp;

impl zed::Extension for ZedFtp {
    fn new() -> Self {
        Self
    }

    fn context_server_command(
        &mut self,
        _id: &ContextServerId,
        _project: &Project,
    ) -> Result<Command> {
        // Rely on host PATH resolution. Users install the binary via
        // `cargo install --path mcp` or a release build.
        Ok(Command {
            command: MCP_BINARY.to_string(),
            args: vec!["serve".to_string()],
            env: vec![],
        })
    }
}

zed::register_extension!(ZedFtp);
