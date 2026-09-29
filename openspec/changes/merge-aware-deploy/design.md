## Context

zed-ftp's upload tools overwrite server files without reading them first. On 2026-09-28, a bulk upload to the tms-falcon staging server replaced `application/models/Mails.php` and reverted a 2FA fix that existed only there. Staging also carries server-only edits, such as a BCC line in `Mailer.php`.

Current upload paths:
- `ftp_upload_file` uploads one file, either the working-tree copy or `HEAD` (with `before_changes`).
- `ftp_deploy` walks `local_root` and uploads every surviving file.
- `ftp_deploy_commits` uploads the working-tree copy of each path changed by the listed commits.
- `ftp_deploy_branch` (`branch_deploy/`) plans `base..head`, uploads exact head blobs over one binary FTP session, and optionally verifies the bytes. `openspec/specs/branch-deployment/spec.md` governs it. The executor is written against the `BranchRemote` and `BlobSource` traits, and in-memory fakes in `branch_deploy/tests.rs` implement both. An ignored integration test in `ftp.rs` runs the real adapter against a pinned Docker FTP server.

Only the branch tool knows a real BASE (the file at `base_ref`), so a true three-way merge fits there. The other tools get a drift check.

## Goals / Non-Goals

**Goals:**
- `ftp_deploy_branch` `mode="merge"`: a three-way merge per file, all-or-nothing uploads, and per-file conflict reports.
- A merge preview through `dry_run=true` that never uploads.
- An opt-in `expect_ref` drift guard on `ftp_upload_file`, `ftp_deploy`, and `ftp_deploy_commits`.
- Docs that make "merge into staging" map to merge mode without ambiguity.
- Callers who opt into nothing see no behavior change.

**Non-Goals:**
- Recording a "last deployed" blob on the server or locally.
- Resolving conflicts automatically, or choosing a side (no `ours` or `theirs` strategy).
- Merging binary files.
- Merge mode for the three non-branch tools.
- A `force` flag.
- CLI commands for the three non-branch tools. They have none today.

## Decisions

### D1: Merge lives only in branch deployment; the other tools get a drift guard
- **Choice**: `mode: overwrite | merge` on `ftp_deploy_branch` and the `deploy-branch` CLI command. Add an optional `expect_ref` on the other three MCP tools.
- **Rationale**: A three-way merge needs a trustworthy BASE, and only the branch range supplies one. The drift guard still stops silent overwrites on the other paths.
- **Alternatives considered**: Merge on all four tools with a `base_ref` on each. Rejected because working-tree uploads with a git BASE are ambiguous, and it quadruples the test surface. Branch only with no guard. Rejected because it leaves the other paths unprotected.

### D2: The per-file decision is a pure function over three optional byte arrays
- **Choice**: A new `branch_deploy/merge.rs` exposes `decide(base: Option<&[u8]>, head: &[u8], server: Option<&[u8]>) -> Result<MergeDecision, BranchDeployError>`. It applies spec rules 2 through 8 in order. `MergeDecision` has the variants `AlreadyDeployed`, `FastForward`, `Merged(Vec<u8>)`, `NewFile`, and `Conflict { reason: ConflictReason, marked_text: Option<Vec<u8>> }`. A file is binary when any present version contains a NUL byte anywhere. Rule 1 (`unchanged_in_range`) is decided earlier, by comparing blob IDs (`base_object_id == object_id`), so no download happens for that file.
- **Rationale**: This keeps the merge logic free of FTP and git plumbing, so the decision table can be tested exhaustively.
- **Alternatives considered**: Deciding inside the executor loop. Rejected because it mixes I/O with rules and makes the table hard to test.

### D3: `git merge-file` is the text merge engine
- **Choice**: `decide` writes the three versions to a `tempfile`-free private temp directory under `std::env::temp_dir()` with unique names. It runs `git merge-file -p --diff3 -L server -L base -L head <server> <base> <head>`, then removes the directory. Exit status 0 means clean, stdout holds the merged bytes, and the order of the arguments makes OURS the server copy. A positive exit status means that many conflicts, and stdout holds the marked text. A negative status or a spawn error is an error. It uses the same `git` executable resolution as `branch_deploy/git.rs`.
- **Rationale**: git is already a hard dependency. `merge-file` is git's own merge algorithm, which gives git's familiar conflict markers.
- **Alternatives considered**: A Rust diff3 crate. Rejected because it adds a dependency, and its merge behavior would differ from git's.
- **Note**: Passing the server copy as the "current" file means that on a clean merge, the server's line endings and layout win where both sides agree.

### D4: Merge execution is two-phase and all-or-nothing
- **Choice**: The existing `execute_deploy` gains a merge path, which is only reachable when `plan.mode == Merge`. It selects binary mode first. Phase 1 marks `unchanged_in_range` files, then downloads every other planned path in Git-path order through `BranchRemote::download_bytes`, and calls `decide`. Phase 2 runs only when the run is not blocked. The blocking causes are those listed in the spec: a conflict, a download failure, a lost or unopened connection, a binary-mode failure, a blob-read failure, a merge-tool failure, or any planning failure. Each cause adds a `FailureRecord`: stage `merge` for conflicts and merge-tool errors, stage `download` for download failures, and the existing stages otherwise. Phase 2 then runs the existing mkdir, upload, and verify loop with each file's chosen bytes, and verification compares with the chosen bytes. A merge preview (`dry_run=true`) runs the same phase 1 through the real connector and returns before phase 2, with would-upload files `planned`. Files that are never decided get `not_decided`. `success` stays `failures.is_empty()`.
- **Rationale**: Uploading some files while others conflict could leave interdependent files half-deployed (brainstorm decision 3).
- **Alternatives considered**: Uploading the clean files and skipping the conflicts. Rejected per brainstorm decision 3.

### D5: The remote gains a download that distinguishes missing from failed
- **Choice**: Add `fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure>` to `BranchRemote`. On `FtpError::UnexpectedResponse` with `Status::FileUnavailable` (550), the `FtpClient` implementation lists the parent directory with `NLST`. When the listing lacks the file name, it returns `Ok(None)`. When the listing contains the name, or the listing itself fails, it returns an `Operation` failure. It routes every other error through the existing `map_branch_ftp_error`, so a lost connection stays `ConnectionLost`. A shared helper `fn download_or_missing(&mut self, path: &str) -> Result<Option<Vec<u8>>, FtpError>` on `FtpClient` implements this rule once for D5 and D8 (spec-reality ruling R1).
- **Rationale**: Treating any error as "missing" would let a transient failure turn a server edit into a `deleted_on_server` conflict. It could also make a missing file look like a `new_file` upload.

### D6: The base blob comes from the base tree
- **Choice**: The planner already resolves both commits. In merge mode it also reads the base commit's tree entry for each planned path (`ls-tree -rz --full-tree <base>`, parsed like `head_tree`), recording `base_object_id: Option<String>` on `PlannedUpload`. The field is `None` when the path is absent at base or is not a regular blob there (mode other than 100644 or 100755). Phase 1 reads the base bytes with the same `BlobSource`.
- **Rationale**: This reuses the existing tree-reading and batch-blob code paths. The base-tree read is skipped entirely in overwrite mode, so overwrite behavior is unchanged.

### D7: The manifest is extended, never reshaped
- **Choice**: Add `mode` and `blocked_by_conflicts` at the top level. Add `UploadStatus::NotNeeded` and `VerificationStatus::NotNeeded`. Add the optional per-upload fields `merge_status` (including `unchanged_in_range` and `not_decided`), `uploaded_from`, `conflict_reason`, `marked_text`, and `marked_text_truncated`, each with `skip_serializing_if = "Option::is_none"`, so overwrite manifests keep today's fields plus `mode` and `blocked_by_conflicts`. `marked_text` is `String::from_utf8_lossy`, cut at 65,536 bytes on a char boundary. `object_id` stays the head blob ID, and `bytes` is the uploaded or planned byte count.
- **Rationale**: Existing consumers keep parsing, and CLI and MCP share one manifest type.

### D8: The drift guard is one shared module
- **Choice**: A new `mcp/src/drift.rs` exposes `resolve_expect_ref(repo_dir: &Path, expect_ref: &str) -> Result<ResolvedRef, DriftError>` and `check_drift<R: DriftRemote>(remote: &mut R, targets: &[DriftTarget], resolved: &ResolvedRef) -> Result<DriftCheck, DriftError>`. `DriftTarget { remote_path, repo_path, upload_bytes }` carries each file's repository-relative path and its exact upload bytes. `DriftCheck` serializes as the spec's drift-check result shape. `DriftRemote` has two methods: `set_binary_mode`, and `download_bytes` with the same 550-plus-listing rule as D5. `FtpClient` implements it through `download_or_missing`. `check_drift` returns `DriftError::Download { remote_path, error }` on the first download failure, and nothing is uploaded after it. `resolve_expect_ref` runs `git rev-parse --verify <ref>^{commit}`. The expected copy comes from `git -C <repo_root> ls-tree -z <commit> -- <repo_path>`: no entry means absent, a regular blob is read with `cat-file blob`, and any other entry type is `DriftError::InvalidArgs`. Unreadable local files and a non-repo `local_root` are also `InvalidArgs`. The deploy functions return a routable error, following the `DeployCommitsError` pattern, so the tools map `InvalidArgs` to an invalid-params error.
- **Wiring**: `ftp_upload_file` finds the repo with `git rev-parse --show-toplevel` from the file's parent, as `before_changes` already does. `ftp_deploy` and `ftp_deploy_commits` find the repo from `local_root`, and each file's `repo_path` is its path relative to the repo root, not `local_root`. The upload bytes are read once and reused for both the check and the upload. `DeployPlan` and `UploadResponse` gain `drift_check: Option<DriftCheck>`, which is omitted when `None`. The check runs before any `mkdir_p`. A refused actual run returns `files_uploaded`, `bytes_uploaded`, and `directories_created` as 0. A refused dry run keeps the normal dry-run counts. `drifted[].remote_path` is the full server path.
- **Rationale**: One implementation serves all three tools, and a fake remote can test it.
- **Alternatives considered**: Reusing `BranchRemote`. Rejected because the non-branch tools use `FtpClient`'s simpler API, and the guard needs only a download.

### D9: Docs are written for the agent that reads the tool list
- **Choice**: The `ftp_deploy_branch` description states: "When the user asks to merge into a server or profile (for example 'merge into staging') or to preserve server-side changes, set mode="merge"; run with dry_run=true first to preview conflicts." The other three descriptions state: "Set expect_ref (e.g. the branch or commit the server was last deployed from) when the user asks to deploy without clobbering server-side changes." The README gains a "Merging into a server" section covering the per-file table, conflict output, the resolution loop (fix on the branch, commit, deploy again), the adjacent-line caveat, and `expect_ref`.
- **Rationale**: Brandon asked that "merge into staging" be understood without re-explaining it.

## Global constraints

- Toolchain: stable Rust from `rust-toolchain`, or else the installed `cargo 1.94.1`. Add no new crate dependencies.
- Unit and contract tests: `cargo test -p zed-ftp-mcp`
- Format: `cargo fmt --all -- --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Disposable FTP tests (Docker required): `ZED_FTP_RUN_FTP_INTEGRATION=1 cargo test -p zed-ftp-mcp ftp::tests::disposable_ -- --ignored --nocapture`
- Default behavior is frozen. With `mode` omitted or `overwrite`, and with `expect_ref` omitted, every tool's inputs, uploads, network activity, and response fields stay as they are today. Exception: branch manifests gain `mode` and `blocked_by_conflicts`.
- An overwrite dry run stays free of blob-content reads, keychain access, and FTP connections. A merge dry run and a drift dry run never upload, create directories, or delete.
- Never merge a file that contains a NUL byte in any version.
- A merge-mode run uploads nothing when any file is `conflict` or `download_failed`, or when the connection was lost.
- Only FTP status 550 means "missing". Every other download error is a failure.
- Do not reconnect after `ConnectionLost`.
- Use one FTP connection per run, and select binary mode before any data transfer in branch deployment.
- Keep deterministic Git-path order in plans, execution, and manifests.
- Parse Git path output with NUL delimiters, and never parse paths by line.
- Never run any test or manual check against the `staging` profile or any non-disposable server. Never print passwords.
- Do not hand-edit `CHANGELOG.md`.

## Risks / Trade-offs

- [Trade-off] `git merge-file` conflicts on adjacent-line edits. → Acceptance rationale: it fails safe, blocking instead of guessing, and the README documents it.
- [Risk] Merge mode adds a download of every planned file before any upload, which is slower on large ranges. → Mitigation: this happens only when merge mode is opted into, and the same session is reused.
- [Risk] The server copy changes between phase 1 and phase 2 (a race). → Mitigation: the window is seconds. Verification checks the uploaded bytes, so a truly concurrent writer is out of scope and documented as such.
- [Risk] Line-ending differences (CRLF on the server) make every file look changed. → Mitigation: they merge as text. When they conflict, the marked text shows it. No normalization is done, because silently rewriting bytes is worse.
- [Risk] Merged bytes are not a commit, so the server no longer equals any git blob. → Mitigation: the manifest's `uploaded_from: merged` makes this visible. Recording merged content back into git is the user's choice.
- [Trade-off] `expect_ref` downloads each target, which is slow for a full `ftp_deploy`. → Acceptance rationale: the guard is opt-in.

- [Risk] Pre-existing: with a subdirectory `local_root`, `ftp_deploy_commits` uploads nothing, because `diff-tree` paths are repo-root-relative. A drift check there would pass vacuously with `checked: 0`. → Mitigation: fixed separately (suggested task "Fix deploy_commits with subdirectory local_root"). The drift guard's `repo_path` uses the repo-root-relative path, so it stays correct once that fix lands.
- [Trade-off] The drift guard compares raw blob bytes with working-tree bytes, so `core.autocrlf` or clean and smudge filters can report false drift. → Acceptance rationale: this fails safe (refusal), and the README documents it.

## Migration Plan

This change needs no data migration. After merging, rebuild and reinstall with `cargo install --path mcp` on the machine that runs the MCP server, then restart the MCP client. Rollback means reinstalling the previous build, and callers who never set `mode` or `expect_ref` see no difference in either direction.

## Open Questions

None. The four brainstorm decisions resolved every fork.
