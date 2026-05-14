# zed-ftp

A Zed extension + MCP server pair that lets you deploy a project to an FTP
server from inside Zed's agent panel.

## How it actually works

Zed extensions run inside a WASM sandbox with no socket access, so this repo
ships **two** crates:

| Crate | Build target | Job |
| --- | --- | --- |
| `extension/` | `wasm32-wasip1` (loaded by Zed) | Tiny shim. Tells Zed how to spawn the MCP binary. |
| `mcp/` | Native binary (`zed-ftp-mcp`, ~3 MB release build) | Real work: FTP, config, keychain, deploy walker. |

Zed's agent calls MCP tools (`ftp_deploy`, `ftp_deploy_commits`, `ftp_test`,
`ftp_list_profiles`, `ftp_list`, `ftp_upload_file`, `ftp_delete_file`); the
binary turns those into FTP commands.

## Install

### 1. Build & install the MCP binary

```sh
cargo install --path mcp
```

This puts `zed-ftp-mcp` on your `PATH` (typically `~/.cargo/bin`).

### 2. Install the extension into Zed

In Zed: **Command Palette → `zed: install dev extension`** and pick the
`extension/` directory in this repo. Zed will compile the WASM module and
register it.

### 3. Configure connection profiles

Create the connections file at the platform's user config dir:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/zed-ftp/connections.toml` |
| Linux | `~/.config/zed-ftp/connections.toml` |
| Windows | `%APPDATA%\zed-ftp\connections.toml` |

Run `zed-ftp-mcp list-profiles` to print the exact path on your machine.
See `connections.example.toml` in this repo for the schema:

```toml
[profiles.staging]
host = "ftp.example.com"
port = 21
user = "deploy"
remote_root = "/var/www/staging"
local_root = "."
passive = true
ignore = ["node_modules", "target", "*.log"]
```

### 4. Store the password in your OS keychain

```sh
zed-ftp-mcp set-password staging
```

You'll be prompted; the password is written to your OS keychain (Keychain on
macOS, Secret Service on Linux, Credential Manager on Windows). Nothing
touches disk in plaintext.

Verify:

```sh
zed-ftp-mcp list-profiles
```

## Use it

Open the Zed agent panel and ask, for example:

> Deploy this project to staging — but show me a dry run first.

The agent will call `ftp_deploy(profile="staging", dry_run=true)`, you review
the file list, then ask it to actually deploy.

Or just call the tools directly without the agent doing any reasoning:

> Run the `ftp_deploy` tool with profile=staging.

### Available tools

| Tool | Purpose |
| --- | --- |
| `ftp_list_profiles` | Enumerate profiles + show which have a stored password |
| `ftp_test` | Connect, log in, return PWD, disconnect |
| `ftp_list` | List a remote directory (relative to `remote_root`) |
| `ftp_download_file` | Download a remote file; returns UTF-8 text or base64 for binary |
| `ftp_upload_file` | Upload one local file; `before_changes=true` uploads the last-committed (git HEAD) version instead of the working tree |
| `ftp_deploy` | Recursive upload of the full local project, gitignore-aware, optional `dry_run` |
| `ftp_deploy_commits` | Upload only the files changed by specific commit SHAs, optional `dry_run` |
| `ftp_mkdir` | Create a directory and any missing parents |
| `ftp_delete_file` | Delete a single remote file |
| `ftp_delete_dir` | Delete an empty remote directory |

## Limitations / scope

- **FTP only.** No FTPS or SFTP yet (suppaftp pulled in without TLS features).
  Adding FTPES is a small surface area change — open an issue.
- **One-shot deploy.** This is push-only; there's no remote browsing UI
  because Zed extensions can't add custom panels. The agent panel is the UI.
- **No mirror sync.** `ftp_deploy` only uploads — it doesn't delete remote
  files that no longer exist locally. Use `ftp_delete_file` to remove stray
  files by hand.
- **WASM extension does no PATH wizardry.** It just returns
  `Command { command: "zed-ftp-mcp", args: ["serve"] }` and trusts Zed to
  resolve via the host shell's `PATH`. If Zed can't find the binary, make
  sure `~/.cargo/bin` is on your shell's PATH and Zed inherits it.

## Repo layout

```
.
├── Cargo.toml                  # workspace
├── connections.example.toml    # template for ~/.config/zed-ftp/connections.toml
├── extension/
│   ├── Cargo.toml
│   ├── extension.toml          # Zed manifest
│   └── src/lib.rs              # WASM shim implementing context_server_command
└── mcp/
    ├── Cargo.toml
    └── src/
        ├── main.rs             # CLI: serve | set-password | list-profiles
        ├── config.rs           # TOML profiles + keychain
        ├── ftp.rs              # suppaftp wrapper
        ├── deploy.rs           # walk + upload, dry_run-aware
        └── tools.rs            # rmcp tool router
```

## Hacking

```sh
# Recompile + reload after edits to the binary
cargo install --path mcp --force

# Or run the server by hand for debugging:
ZED_FTP_LOG=debug zed-ftp-mcp serve   # speaks JSON-RPC on stdio

# Type-check the WASM extension
cargo check -p zed-ftp --target wasm32-wasip1
```

## License

MIT.
