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

### Plan a Git worktree range

Use `deploy-branch` to deploy the committed files from a selected Git
worktree. The command requires the absolute path to the exact worktree root
and a base ref. The head ref defaults to `HEAD`. Pass `--dry-run` to preview
the manifest. In the default overwrite mode, a dry run does not access saved
credentials, Git blob contents, or FTP. In merge mode, a dry run connects to
the server to preview the merge and never writes (see "Merging into a server").

```sh
zed-ftp-mcp deploy-branch staging \
	--repo-root /absolute/path/to/repository \
	--base origin/main \
	--dry-run
```

The MCP equivalent is `ftp_deploy_branch`. It accepts `profile`, `repo_root`,
and `base_ref`, plus optional `head_ref`, `verify`, `dry_run`, and `mode`
fields. `mode` is `overwrite` (the default) or `merge`.

The overwrite dry-run manifest lists the union of paths touched by every
commit in the range. Each surviving path uses the blob from the resolved head commit, even
when the worktree is dirty or the profile's `local_root` and ignore rules point
somewhere else. The manifest records the dirty state and reports removed Git
paths. A reported removal never deletes a remote file.

Without `--dry-run`, zed-ftp opens one FTP session, selects binary transfer
mode, uploads each committed blob in Git-path order, and verifies every upload
by downloading and comparing its complete byte stream in that same session.
Verification is on by default. Pass `--no-verify` only when you intentionally
want to skip the download comparison.

An execution manifest records each upload and verification status. A mismatch
or per-file FTP failure leaves the other planned paths eligible to run, while a
lost connection marks the remaining paths as not attempted. The CLI always
prints this complete manifest and exits nonzero when any upload or verification
does not succeed. The MCP tool returns the same unsuccessful manifest as data.

### Merging into a server

By default `deploy-branch` overwrites. It uploads every file in the range as
the head commit has it, so any edit someone made to that file on the server is
lost. To keep those edits, pass `--mode merge`. The MCP equivalent is
`mode="merge"` on `ftp_deploy_branch`.

```sh
zed-ftp-mcp deploy-branch staging \
	--repo-root /absolute/path/to/repository \
	--base origin/main \
	--mode merge
```

In merge mode the base ref is the common ancestor. For each file in the range,
zed-ftp downloads the server copy and compares three versions: the file at the
base commit, the file at the head commit, and the file on the server. The
result is the file's `merge_status`.

| `merge_status` | When | What happens |
| --- | --- | --- |
| `unchanged_in_range` | Base and head hold the same content. | Nothing is downloaded or uploaded. |
| `already_deployed` | The server copy equals head. | Nothing is uploaded. |
| `fast_forward` | The server copy equals base. | Head is uploaded as it is. |
| `merged` | Server and head both changed the file, and the changes combine cleanly. | The merged content is uploaded. |
| `new_file` | The file is new in head and absent on the server. | Head is uploaded. |
| `conflict` | The changes cannot be combined. `conflict_reason` says why. | The whole run is blocked. |
| `download_failed` | The server copy could not be read for a reason other than being absent. | The whole run is blocked. |
| `not_decided` | The run was blocked before this file was checked. | Nothing is uploaded. |

A `conflict` has one of four reasons in `conflict_reason`:

- `text_conflict`: server and head changed nearby lines of a text file.
- `binary_changed`: a version contains a NUL byte and the server copy differs from both base and head.
- `deleted_on_server`: the file exists at base but is missing on the server.
- `added_on_both`: the file is new in head, and the server already has a different copy.

Merge mode is all or nothing. It decides every file before it uploads
anything. If any file conflicts or fails to download, or the connection drops
during the decisions, zed-ftp uploads nothing. The manifest then has
`blocked_by_conflicts: true` and `success: false`, and the CLI exits nonzero.
In a real run, files that would have uploaded show
`upload_status: not_attempted`. Each cause has an entry in `failures`.

For a `text_conflict`, the upload result carries `marked_text`. It holds the
file with `<<<<<<< server`, `||||||| base`, `=======`, and `>>>>>>> head`
markers, so you can see both edits and the original. Non-UTF-8 bytes appear as
replacement characters. `marked_text` stops at 65,536 bytes and sets
`marked_text_truncated` to true when it does. The server copy is never
changed by a blocked run.

To resolve a conflict, fix it on the branch and deploy again.

1. Read `marked_text` and the `conflict_reason` for each conflicting file.
2. Change the file on the branch so it includes the server-side edit or drops it on purpose.
3. Commit the change.
4. Run the same `deploy-branch --mode merge` command again.

A merged file uploads content that no commit contains. Its `object_id` is still
the head blob, and `bytes` is the size of the merged content. `uploaded_from`
is `merged` for those files and `head_blob` for the others, and verification
compares the server against the uploaded content.

Merging works on lines. When the server edit and the head edit touch adjacent
lines, Git treats them as one overlapping change and reports a
`text_conflict`, even if a person would see two independent edits. A clean
`merged` result also means only that the two edits did not overlap, not that
the combined file is correct. Review the file after a merge.

### Preview a merge

Add `--dry-run` to see every file's `merge_status` before anything changes on
the server. A merge preview connects to the server, but it never writes.

```sh
zed-ftp-mcp deploy-branch staging \
	--repo-root /absolute/path/to/repository \
	--base origin/main \
	--mode merge \
	--dry-run
```

The preview reads blob contents and saved credentials, opens one FTP session in
binary mode, and downloads each server copy that needs a decision. It does not
upload, create directories, verify, or delete. The manifest has `dry_run: true`
and the same merge fields as a real run. A file that would upload shows
`upload_status: planned`. When a file conflicts, the manifest has
`blocked_by_conflicts: true` and `success: false`, and the CLI exits nonzero.
The files that would have uploaded still show `planned`, so you can see what a
real run would send once the conflict is resolved. The conflicting file shows
`not_attempted`. Run the same command without `--dry-run` to deploy.

### Delete a reported branch path explicitly

`deploy-branch` never deletes remote files. To delete a reported path, call the
separate `delete-branch-files` command with the complete 40-character lowercase
base and head commit IDs from the deployment manifest, every exact Git path to
remove, and a non-empty reason.

```sh
zed-ftp-mcp delete-branch-files staging \
	--repo-root /absolute/path/to/repository \
	--base-commit 0123456789abcdef0123456789abcdef01234567 \
	--head-commit 89abcdef0123456789abcdef0123456789abcdef \
	--path old-file.txt \
	--reason "The file was intentionally removed from this release"
```

The MCP equivalent is `ftp_delete_branch_files` with `profile`, `repo_root`,
`base_commit`, `head_commit`, `paths`, `reason`, and optional `dry_run`.
An agent must explain why deletion is needed and receive user approval before
making this separate explicit call.

Preflight recomputes the deleted set from those pinned commits before reading
credentials or contacting FTP. It rejects the complete request when any path
is not deleted in that range, is unsafe, is non-ASCII, is duplicated, or
ASCII-case-collides with another requested path or a surviving head blob. This
prevents a case-only rename from deleting its replacement on a case-insensitive
server. Pass `--dry-run` to return the authorized planned paths without remote
access. A deletion manifest records the approved `repository_root`, blocked,
deleted, failed, and not-attempted paths. A normal FTP operation failure continues to the next approved path. A
lost connection stops later paths without reconnecting and makes the command
exit nonzero.

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
| `ftp_deploy_branch` | Plan a committed range from an explicit Git worktree, optional `dry_run`. `mode="merge"` merges into server-side edits, and a merge `dry_run` connects to preview without writing |
| `ftp_delete_branch_files` | Delete exact files from a pinned branch range after explicit approval |
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
