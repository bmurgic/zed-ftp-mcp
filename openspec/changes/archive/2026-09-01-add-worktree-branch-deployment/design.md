## Context

See [proposal.md](proposal.md) for motivation and [specs/branch-deployment/spec.md](specs/branch-deployment/spec.md) for the behavior contract.

The current commit deployment pipeline in `mcp/src/deploy.rs` combines Git path discovery, profile-root filesystem reads, ignore filtering, and FTP upload. Reusing it would preserve the stale-worktree failure this change addresses. `mcp/src/ftp.rs` wraps a blocking `suppaftp` stream behind `anyhow::Result`, while the new executor needs typed connection-loss handling and streamed remote comparison. The CLI and MCP server already share the same Rust crate and profile configuration.

The selected repository may be a linked Git worktree and may be dirty. Deployment must use committed objects, not filesystem contents. Git path output may contain whitespace or newlines, so parsing line-delimited porcelain output is not safe. FTP servers may also be case-insensitive even when Git is not.

## Goals / Non-Goals

**Goals:**

- Keep branch planning independent from FTP execution so it can be tested with temporary repositories and reused by CLI and MCP.
- Make all planning and execution order deterministic.
- Keep at most one committed blob in memory while uploading and verifying it.
- Preserve typed distinctions between recoverable operation failures and a lost FTP session.
- Prove that deletion requests are exactly tied to pinned commits and are safe before credentials or network access.

**Non-Goals:**

- Refactor or replace the existing deploy pipeline.
- Export a commit into a temporary directory.
- Synchronize an entire remote tree or infer remote deletions from server listings.
- Automatically delete paths reported by branch deployment.
- Retry or reconnect after a lost FTP connection.
- Support non-ASCII remote deletion in the first version.

## Decisions

### 1. Add an isolated planner and executor

Add the following module boundary:

```text
mcp/src/branch_deploy/
├── mod.rs
├── git.rs
├── execute.rs
└── tests.rs
```

`git.rs` owns repository validation, ref resolution, range enumeration, path unioning, head-tree lookup, deletion-set computation, and committed blob reads. `execute.rs` owns manifest state transitions and remote operations. `mod.rs` exposes request, plan, manifest, status, failure, and orchestration types to the CLI and MCP layer. Unit and fixture coverage lives in `tests.rs`, with adapter-specific tests remaining beside the FTP adapter where useful.

The CLI and MCP wrappers load the same profile and call the same orchestration functions. They do not reimplement planning or execution rules.

Alternative considered: generalize `deploy.rs` around a common file source. Rejected because the existing filesystem pipeline applies ignore rules and current-file semantics that are deliberately absent here. A shared abstraction would either leak those rules into branch deployment or make the established path riskier to change.

Alternative considered: export the head commit to a temporary directory and call the existing uploader. Rejected because it adds filesystem cleanup, free-space, metadata, and path-canonicalization failure modes without improving the Git object contract.

### 2. Model planning as metadata first, content later

The planner produces a `BranchDeployPlan` containing resolved commits, dirty state, deterministic upload metadata, and deleted-path reports. Upload metadata contains the Git path, mapped remote path, blob object ID, and byte count, but not blob contents.

Dry run ends after metadata planning. Actual execution starts one long-lived `git cat-file --batch` child and requests one blob at a time. The executor retains that blob only through its upload and optional comparison, then releases it before requesting the next blob. This prevents the full deployment payload from accumulating in memory and ensures dry run never reads blob contents.

Blob sizes used in plans come from object metadata through `git cat-file --batch-check`, not from working-tree files. Profile configuration may be read for remote-root mapping during dry run, but keychain password access remains inside FTP connection creation.

Alternative considered: read every blob during planning. Rejected because it violates dry-run behavior and scales memory with the deployment payload.

### 3. Use NUL-delimited Git plumbing and explicit commit semantics

All Git commands run with `git -C <repo_root>` and no shell interpolation. Repository validation compares the normalized supplied absolute path with `git rev-parse --show-toplevel`; a mismatch is invalid input. Refs resolve with `git rev-parse --verify <ref>^{commit}` and are stored as full commit IDs.

The range is enumerated with the equivalent of `git rev-list --reverse --topo-order <base>..<head>`. Each commit is inspected with NUL-delimited `git diff-tree -z --root --no-commit-id -r --name-status --no-renames`; merge commits are compared with their first parent. Rename detection is disabled so a rename contributes a removed source path and an added destination path deterministically. A sorted byte-keyed map deduplicates touched paths, and final manifest order follows exact Git path byte order after path validation.

One recursive NUL-delimited `git ls-tree -rz --full-tree <head>` builds the head lookup. Surviving regular blobs become upload entries. Touched paths absent from that lookup become deletion reports. Git entries that cannot be represented safely as UTF-8 manifest and FTP paths become planning failures. Submodules and other non-blob entries do not become uploads.

Dirty state comes from a NUL-delimited status query and is reported only. It never changes selected blob IDs or bytes.

Alternative considered: use one net diff from base to head. Rejected because it loses paths changed and restored within the range. Alternative considered: follow only first-parent history. Rejected because it omits commits introduced through merges. The chosen rule enumerates the full reachable range and uses first-parent comparison only for each merge commit's own resolution delta.

### 4. Share one narrow remote protocol between the real adapter and tests

The generic executor depends on this synchronous protocol:

```rust
trait BranchRemote {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure>;
    fn mkdir_p(&mut self, path: &str) -> Result<(), RemoteFailure>;
    fn upload_bytes(&mut self, path: &str, bytes: &[u8])
        -> Result<u64, RemoteFailure>;
    fn compare_remote_bytes(
        &mut self,
        path: &str,
        expected: &[u8],
    ) -> Result<RemoteComparison, RemoteFailure>;
    fn delete_file(&mut self, path: &str) -> Result<(), RemoteFailure>;
}
```

`RemoteComparison` contains `matches` and `bytes_read`. `RemoteFailure` contains `RemoteFailureKind::Operation` or `RemoteFailureKind::ConnectionLost` plus safe diagnostic text. The in-memory fake records call order and can inject either failure kind at every operation.

`FtpClient` implements this protocol through branch-specific typed adapter methods. If the current `anyhow` context chain cannot reliably expose the underlying `suppaftp::FtpError`, the adapter adds typed methods without changing existing public method signatures. Connection and transport failures classify as `ConnectionLost`; valid server rejections and path-specific failures classify as `Operation`. The wrapper owns connection creation and best-effort `quit`; connection factories and lifecycle methods are not part of the generic trait.

The executor calls `set_binary_mode` before directory, upload, retrieval, or deletion work. `compare_remote_bytes` streams RETR data, compares incrementally, counts all bytes, and continues draining the data stream after the first mismatch so the FTP command can finalize cleanly.

Alternative considered: expose `suppaftp` types throughout branch deployment. Rejected because it couples manifest control flow and tests to one transport library. Alternative considered: put connection creation and shutdown in the trait. Rejected because the executor needs one already-created session and tests only need operation behavior.

### 5. Use explicit manifest state machines

Planning failures that make a request invalid, such as a wrong root or unresolved ref, return invalid parameters before execution. Once a valid plan enters execution, every outcome is represented in a complete manifest.

The deployment manifest contains:

- `success` and `profile`
- `repository { root, dirty }`
- `refs { base { requested, commit }, head { requested, commit } }`
- `merge_rule: "first_parent"`, `dry_run`, and `verify`
- counts for commits, touched paths, planned uploads, uploaded files, verified files, reported deletions, and failures
- ordered upload entries with Git path, remote path, blob ID, bytes, upload status, and verification status
- ordered deleted entries with `requires_explicit_call` or a blocking reason
- ordered failures with stage, optional exact Git path, and diagnostic text

Statuses are closed enums serialized with stable snake-case names. Dry run returns the same structure with planned or not-requested statuses. Per-operation failures advance to the next path. Connection loss changes every remaining entry to `not_attempted` without reconnecting. Any failure or verification mismatch makes `success` false. MCP returns this unsuccessful manifest as data; CLI serializes it and exits nonzero.

Deletion uses a parallel manifest with the pinned commits, caller reason, ordered requested paths, blocked path reasons, per-path deletion statuses, and failures. Preflight rejection is still structured so the caller sees every blocked path rather than one opaque error.

Alternative considered: fail fast with `anyhow::Error` after the first remote failure. Rejected because it discards deterministic partial results and prevents a caller from knowing which exact paths remain untouched.

### 6. Make remote path mapping lexical and platform-independent

Git paths are repository-relative byte sequences. After UTF-8 validation, a shared lexical validator rejects empty paths, absolute paths, empty components, `.` or `..` components, backslashes, control characters, and any path whose mapped remote target is equal to or outside the configured remote root. No per-file `canonicalize` or local filesystem lookup occurs.

Remote targets are formed by joining the profile's normalized remote root with validated Git components using `/`. Profile `local_root`, `.gitignore`, and profile ignore patterns do not participate because the capability deploys every touched surviving committed blob.

Alternative considered: reuse `PathBuf` joins and canonicalization. Rejected because host filesystem semantics differ from Git and canonicalization would consult mutable working-tree state.

### 7. Treat deletion as pinned-set authorization

The deletion operation accepts full base and head commit IDs, exact requested Git paths, and a non-empty reason. It resolves both IDs as commits and requires their canonical full IDs to equal the caller values. It then recomputes the deleted set using the same range and head-tree rules as deployment. Requested paths must be a subset of that set.

Preflight validates the complete request before keychain or FTP access. A path is blocked when it fails lexical validation, is not valid UTF-8, contains non-ASCII bytes, maps to the remote root itself, or ASCII-case-collides with another requested deletion or any surviving head blob. Comparing against surviving head blobs prevents a case-only rename from deleting the replacement on a case-insensitive server. All blocked paths and reasons are collected, then the entire call is rejected with no remote operations.

After successful preflight, deletion uses one binary-mode session and deterministic exact-path order. An `Operation` failure is recorded and deletion continues. `ConnectionLost` stops work and marks the remaining entries `not_attempted`. No branch deployment path can call this executor implicitly.

Alternative considered: accept branch names again at deletion time. Rejected because mutable refs could authorize a different set between approval and execution. Alternative considered: permit Unicode after normalization. Rejected for the first version because Git, JSON, and FTP server case and normalization rules cannot be proven equivalent.

### 8. Keep CLI and MCP contracts thin and aligned

Add CLI commands equivalent to:

```text
zed-ftp-mcp deploy-branch <profile> --repo-root <absolute-path> --base <ref> [--head <ref>] [--no-verify] [--dry-run]
zed-ftp-mcp delete-branch-files <profile> --repo-root <absolute-path> --base-commit <id> --head-commit <id> --path <git-path>... --reason <text> [--dry-run]
```

Add MCP tools `ftp_deploy_branch` and `ftp_delete_branch_files` with the same inputs and defaults. Schema types live with the shared branch-deployment requests and manifests, while `tools.rs` only maps tool input, blocking execution, invalid-parameter errors, and structured results.

Alternative considered: make the CLI call the MCP server. Rejected because both interfaces are already in the same process and direct reuse avoids protocol and startup overhead.

## Risks / Trade-offs

- [A very large blob must fit in memory during its own upload and comparison] → Hold only one blob, release it immediately, and document that branch deployment is file-bounded rather than stream-through from Git.
- [Walking the full head tree is more work than querying only touched paths] → Use one process and one parse for deterministic type and collision checks; this also makes case-only deletion safety possible.
- [FTP servers return inconsistent response text and codes] → Preserve typed transport errors where available, test classification at the adapter boundary, and treat uncertain session health as connection loss rather than reconnecting.
- [A server may accept an upload but return a verification mismatch] → Record both statuses, mark the manifest unsuccessful, and leave retry or rollback to a later explicit invocation.
- [Deployment can be partially applied before a failure] → Return exact per-path results and never claim atomic remote deployment.
- [Non-UTF-8 Git paths cannot be represented in JSON or addressed portably over FTP] → Record a planning failure and do not upload or delete those paths.
- [Blocking Git and FTP work can stall async request handling] → Keep the existing `spawn_blocking` boundary in MCP handlers and use synchronous internals beneath it.

## Migration Plan

1. Add the planner, shared data types, in-memory remote, and fixture tests without registering public commands.
2. Add the typed FTP adapter and disposable-server integration test.
3. Register CLI and MCP deployment surfaces, then register the separate deletion surfaces.
4. Update README examples and the explicit deletion approval workflow.
5. Run formatting, warning-free linting, workspace tests, WebAssembly checks, and the opt-in disposable FTP integration test.

Rollback removes the two new CLI commands, two new MCP tools, and isolated `branch_deploy` module. Existing FTP and deployment paths remain unchanged, so no data or configuration migration is required.
