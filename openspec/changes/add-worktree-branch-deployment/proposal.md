## Why

`ftp_deploy_commits` reads files from a profile's fixed `local_root`, so it can deploy stale bytes when the selected branch lives in another Git worktree. A branch deployment also needs the union of paths touched across the full commit range, exact committed blobs, and built-in verification so an agent does not need hundreds of manual upload and download calls.

## What Changes

- Add one CLI command and one MCP tool that deploy a caller-selected Git worktree and commit range.
- Build the upload set from every commit in `base_ref..head_ref`, then upload each surviving `head_ref` blob once.
- Read file names and bytes through Git plumbing commands without resolving individual files through the local filesystem.
- Verify uploaded bytes inside the deployment operation through the same binary-mode FTP connection.
- Return one structured manifest for dry runs and actual deployments, including resolved refs, dirty state, counts, path results, deleted-path reports, and failures.
- Report deleted Git paths without removing remote files.
- Add a separate explicit deletion command and MCP tool that accepts pinned commits, approved paths, and a reason, then blocks unsafe or case-colliding deletions before contacting FTP.
- Preserve the behavior of `ftp_deploy`, `ftp_deploy_commits`, and the existing single-file tools.

## Capabilities

### New Capabilities

- `branch-deployment`: Deploy and verify committed Git branch state from a selected worktree, report removed paths, and handle separately approved remote deletion safely.

### Modified Capabilities

None.

## Impact

- Adds branch deployment modules under `mcp/src/branch_deploy/`.
- Adds CLI commands in `mcp/src/main.rs` and MCP contracts in `mcp/src/tools.rs`.
- Extends `mcp/src/ftp.rs` with a binary-mode branch-deployment adapter while preserving existing methods.
- Extends schema coverage, Git fixture tests, in-memory remote tests, and one opt-in disposable FTP integration test.
- Updates the README with the new commands, MCP tools, result behavior, and deletion approval flow.
