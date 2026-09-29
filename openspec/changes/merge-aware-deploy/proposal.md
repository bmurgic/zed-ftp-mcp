## Why

Every upload path overwrites the server copy without checking it first. On 2026-09-28, a bulk upload to staging replaced `application/models/Mails.php` and reverted a 2FA email fix that existed only on the server. Staging also carries server-only edits, such as a BCC line in `Mailer.php`, that any overwrite would erase without a warning.

## What Changes

- `ftp_deploy_branch` and the `deploy-branch` CLI command gain `mode` (`overwrite` | `merge`). The default is `overwrite`, which keeps today's behavior.
- Merge mode three-way merges each planned file. BASE is the blob at the base commit, OURS is the head blob, and THEIRS is the server's current copy. The tool uploads only when every file resolves cleanly. One conflict or download failure blocks every upload.
- Merge mode never merges a binary file. A binary file uploads only when the server copy equals BASE.
- `dry_run=true` with merge mode becomes an online preview. It reads credentials and downloads server copies, but it never uploads.
- With merge mode, verification compares the server bytes with the bytes actually uploaded, which may be merged bytes.
- The branch manifest gains `mode`, `blocked_by_conflicts`, per-file `merge_status`, `uploaded_from`, and conflict-marked text.
- `ftp_upload_file`, `ftp_deploy`, and `ftp_deploy_commits` gain an optional `expect_ref`. When it is set, any server file that matches neither the file at that ref nor the bytes about to be uploaded refuses the whole run. When it is not set, behavior is unchanged.
- Tool descriptions and the README tell an agent to use merge mode when the user asks to merge into a server.

## Capabilities

### New Capabilities
- `server-drift-guard`: an opt-in `expect_ref` check on the single-file, directory, and commit upload tools that refuses to overwrite drifted server files.

### Modified Capabilities
- `branch-deployment`: adds merge mode, and amends three requirements for merge mode only: the upload source ("exact committed head blobs"), the verification target ("compares complete remote bytes"), and the offline dry run ("performs no secret, content, or network access"). It also amends "Existing FTP operations remain compatible" to permit the opt-in `expect_ref` input.

## Assumptions

- Scope, the dry-run amendment, all-or-nothing blocking, and `expect_ref` with no defaults follow brainstorm decisions 1 through 4.
- A file is binary when any of its versions contains a NUL byte anywhere. git checks only the first 8,000 bytes, so this test is stricter than git's.
- `git merge-file` with default settings is the merge engine. Edits on adjacent lines count as a conflict.
- Conflict-marked text in the manifest is capped at 64 KiB per file.
- Merge mode is also exposed in the `deploy-branch` CLI, because the spec requires CLI and MCP parity.
- `expect_ref` is exposed on the MCP tools only, because those three tools have no CLI commands.
- A server path counts as missing only when a download returns FTP 550 and a listing of the parent directory lacks the name (spec-reality ruling R1). Every other download error counts as a download failure.

## Success Criteria

- A merge deploy of a branch that touches a server-edited file keeps the server-only lines, or blocks with a listed conflict. It never silently reverts them.
- A merge dry run reports every file's merge status and makes no upload.

## Impact

- Code: `mcp/src/branch_deploy/` (planner, executor, new merge engine), `mcp/src/ftp.rs` (download on `BranchRemote`), `mcp/src/tools.rs`, `mcp/src/main.rs`, `mcp/src/deploy.rs`, and a new shared drift module.
- Specs: `openspec/specs/branch-deployment/spec.md` (modified), `server-drift-guard` (new).
- Docs: `README.md` and the MCP tool descriptions.
- Dependencies: none new. It uses the existing `git` executable.
