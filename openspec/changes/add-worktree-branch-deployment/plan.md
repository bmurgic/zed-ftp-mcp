# Worktree branch deployment implementation plan

> **For agentic workers:** REQUIRED CONTROLLER: Use `gauntlet-driven-development`. Do not use stock subagent-driven development or `executing-plans` for this OpenSpec change. Complete one task through Cleaner, Architect, Security Reviewer, Hardener, and QA before starting the next task.

**Goal:** Add deterministic deployment of exact committed Git branch state from any selected worktree, verify uploads inside the operation, and require a separate pinned call for every remote deletion.

**Architecture:** Add an isolated `branch_deploy` planner and executor beside the current filesystem deployer. Git plumbing produces ordered metadata and one `cat-file --batch` process supplies committed bytes. CLI and MCP wrappers share the same orchestration code, while a narrow `BranchRemote` trait separates execution policy from `suppaftp`.

**Tech stack:** Rust 2021, clap, rmcp, serde, schemars, suppaftp 8, tokio, temporary Git repositories, and an opt-in pinned Docker FTP server.

**Spec:** `openspec/changes/add-worktree-branch-deployment/specs/branch-deployment/spec.md`

**Design:** `openspec/changes/add-worktree-branch-deployment/design.md`

**Acceptance scenarios:** `openspec/changes/add-worktree-branch-deployment/scenarios.md`

**QA procedures:** `openspec/changes/add-worktree-branch-deployment/qa.md`

## Global constraints

- Keep `mcp/src/deploy.rs` and all existing CLI and MCP contracts unchanged.
- Use exact committed blobs from the resolved head commit. Never read deploy bytes from the working tree or profile `local_root`.
- Enumerate every commit in `base_ref..head_ref`. Compare each merge commit with its first parent without dropping merged-side commits from the range.
- Parse Git path output with NUL delimiters. Never parse paths by line.
- Keep dry run free of blob-content reads, keychain access, FTP connections, uploads, downloads, and deletions.
- Use one `git cat-file --batch` process and one FTP connection for each actual deployment.
- Select FTP binary mode before any data operation.
- Never delete a remote file during branch deployment.
- Require a separate deletion call with full commit IDs, exact paths, and a non-empty reason.
- Reject the complete deletion request before credential or network access when any path fails preflight.
- Do not reconnect after `ConnectionLost`.
- Preserve deterministic exact Git-path order in plans, execution, and manifests.
- Do not ask an agent to compare uploaded files manually. Automated byte comparison is the acceptance evidence.

## File map

- `mcp/src/branch_deploy/mod.rs`: public requests, plans, manifests, statuses, orchestration, and lexical remote-path mapping.
- `mcp/src/branch_deploy/git.rs`: Git command runner, worktree and ref validation, commit enumeration, tree lookup, deletion-set calculation, and batch blob reader.
- `mcp/src/branch_deploy/execute.rs`: generic deployment and deletion state machines over `BranchRemote`.
- `mcp/src/branch_deploy/tests.rs`: temporary Git fixtures, in-memory remote, planner tests, executor tests, and deletion tests.
- `mcp/src/ftp.rs`: typed branch adapter, binary mode, streamed comparison, error classification, and opt-in disposable-server tests.
- `mcp/src/main.rs`: two clap subcommands and shared manifest output and exit handling.
- `mcp/src/tools.rs`: two MCP request types and thin handlers.
- `mcp/src/schema.rs`: MCP output-schema coverage for branch manifests.
- `mcp/Cargo.toml`: `tempfile` dev dependency only. The live FTP test uses the installed Docker CLI and adds no runtime dependency.
- `README.md`: branch deployment, verification, dry-run, manifest, and explicit deletion workflow.

---

## Task 1: Deterministic branch plan and dry run

**GDD slice:** 1

**Behavior references:** S1.1 through S1.8 in `scenarios.md`; requirements "Branch deployment accepts an explicit repository and commit range" through "Dry run performs no secret, content, or network access" in the OpenSpec capability spec.

**QA reference:** `qa.md#slice-1-qa-deterministic-plan-and-dry-run`

**Files:**

- Create: `mcp/src/branch_deploy/mod.rs`
- Create: `mcp/src/branch_deploy/git.rs`
- Create: `mcp/src/branch_deploy/tests.rs`
- Modify: `mcp/src/main.rs`
- Modify: `mcp/src/tools.rs`
- Modify: `mcp/src/schema.rs`
- Modify: `mcp/Cargo.toml`
- Modify: `README.md`

**Consumes:** `config::Profile`, profile `remote_root`, clap subcommands, rmcp tool handlers, serde, schemars, and the `remove_unsigned_integer_format` schema transform.

**Produces:**

```rust
pub struct DeployBranchRequest {
    pub profile: String,
    pub repo_root: String,
    pub base_ref: String,
    pub head_ref: String,
    pub verify: bool,
    pub dry_run: bool,
}

pub struct BranchDeployPlan {
    pub profile: String,
    pub repository: RepositorySummary,
    pub refs: ResolvedRefs,
    pub commits: Vec<String>,
    pub uploads: Vec<PlannedUpload>,
    pub deleted: Vec<DeletedPathResult>,
    pub failures: Vec<FailureRecord>,
}

pub enum BranchDeployError {
    InvalidArgs(String),
    Other(anyhow::Error),
}

pub fn plan_branch(
    request: &DeployBranchRequest,
    profile: &Profile,
) -> Result<BranchDeployPlan, BranchDeployError>;

pub fn dry_run_manifest(plan: BranchDeployPlan, verify: bool) -> BranchDeployManifest;
```

- [x] **1.1 Add the failing contract and path tests.**

  Add `#[cfg(test)] mod tests;` to `branch_deploy/mod.rs`. In `tests.rs`, add tests with these exact names:

  ```rust
  #[test] fn planner_contract_serializes_stable_manifest_fields() {}
  #[test] fn planner_path_maps_under_remote_root() {}
  #[test] fn planner_path_rejects_absolute_dot_backslash_and_control_components() {}
  #[cfg(unix)] #[test] fn planner_path_rejects_non_utf8() {}
  ```

  Assert the complete JSON keys from the approved manifest, snake-case enum values, exact `/remote/root/path` mapping, and one failure for each unsafe input. Do not use snapshot files.

- [x] **1.2 Run the focused tests and capture the expected red state.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp planner_ -- --nocapture
  ```

  Expected result: compilation fails because `branch_deploy` and its contract types do not exist.

- [x] **1.3 Add the shared contracts and lexical mapper.**

  Define these serialized statuses in `mod.rs`:

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
  #[serde(rename_all = "snake_case")]
  pub enum UploadStatus { Planned, Uploaded, Failed, NotAttempted }

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
  #[serde(rename_all = "snake_case")]
  pub enum VerificationStatus { Planned, Verified, Mismatch, NotRequested, Failed, NotAttempted }

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
  #[serde(rename_all = "snake_case")]
  pub enum DeletedPathStatus { RequiresExplicitCall, BlockedCaseCollision }
  ```

  Add `RepositorySummary`, `RequestedAndResolvedRef`, `ResolvedRefs`, `ManifestCounts`, `PlannedUpload`, `UploadResult`, `DeletedPathResult`, `FailureRecord`, and `BranchDeployManifest`. Apply `remove_unsigned_integer_format` to every unsigned schema field.

  Implement one mapper with this contract:

  ```rust
  fn map_remote_path(remote_root: &str, git_path: &[u8]) -> Result<(String, String), PathFailure>;
  ```

  Require valid UTF-8, a non-empty relative path, non-empty components, no `.` or `..`, no backslash or control character, and a mapped target strictly below the normalized remote root. Return both the Git path and remote path as owned strings.

- [x] **1.4 Run the contract and path tests until they pass.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp planner_ -- --nocapture
  ```

  Expected result: all new tests pass and existing schema tests remain green.

- [x] **1.5 Add the temporary Git fixture and failing planner tests.**

  Add `tempfile = "3"` under `[dev-dependencies]`. Implement `TestRepo` with `tempfile::TempDir` and a `git(&[&str]) -> Output` helper that sets repository-local `user.name` and `user.email`.

  Add test names grouped by the `planner_` prefix:

  ```rust
  #[test] fn planner_rejects_relative_subdirectory_and_missing_ref() {}
  #[test] fn planner_defaults_head_to_head_and_records_full_ids() {}
  #[test] fn planner_unions_intermediate_repeated_and_restored_paths() {}
  #[test] fn planner_keeps_merged_side_commits_and_first_parent_resolution() {}
  #[test] fn planner_uses_dirty_head_blob_metadata_and_ignores_profile_filters() {}
  #[test] fn planner_reports_deleted_recreated_submodule_and_unsafe_entries() {}
  #[test] fn planner_orders_exact_git_paths_deterministically() {}
  ```

  Create real commits for every case. Use `git hash-object` and `git cat-file -s` as independent expected values. For the dirty case, overwrite the working-tree file after committing and assert the planned object ID and size still match `HEAD:path`.

- [x] **1.6 Run the planner tests and capture the expected red state.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp branch_deploy::tests::planner -- --nocapture
  ```

  Expected result: tests fail because Git planning functions do not exist.

- [x] **1.7 Implement the Git planner with NUL-delimited plumbing.**

  Add a command helper that always invokes `git -C <repo_root>` without a shell. Map `ErrorKind::NotFound` to `InvalidArgs("`git` was not found on PATH")`. Include stderr in other invalid Git-command messages.

  Use these command sequences:

  ```text
  rev-parse --show-toplevel
  rev-parse --verify <base>^{commit}
  rev-parse --verify <head>^{commit}
  rev-list --reverse --topo-order <base>..<head>
  rev-list --parents -n 1 <commit>
  diff-tree --root --no-commit-id -r --name-status -z --no-renames <root-commit>
  diff-tree --no-commit-id -r --name-status -z --no-renames <first-parent> <commit>
  ls-tree -rz -l --full-tree <head-commit>
  status --porcelain=v1 -z --untracked-files=normal
  ```

  Compare the normalized absolute request path with `--show-toplevel`. Parse status and path records as bytes. Store touched paths in `BTreeSet<Vec<u8>>`. Parse the full head tree into a byte-keyed map of mode, type, object ID, and size. Treat only `type=blob` as uploadable. Report missing touched paths as deleted. Report submodules and unrepresentable or unsafe paths as planning failures.

  Mark a deleted entry `blocked_case_collision` when its ASCII-folded path matches a surviving head blob. Otherwise mark it `requires_explicit_call`. Do not call any delete method.

- [x] **1.8 Run all planner tests until they pass.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp branch_deploy::tests::planner -- --nocapture
  ```

  Expected result: every Git fixture passes twice in the same process without order drift.

- [x] **1.9 Add the failing dry-run side-effect test.**

  Add counters at orchestration seams under test. The test must assert this exact result after `dry_run_manifest`:

  ```rust
  assert_eq!(effects.blob_reads(), 0);
  assert_eq!(effects.credential_reads(), 0);
  assert_eq!(effects.remote_connections(), 0);
  assert_eq!(manifest.uploads.iter().map(|u| u.upload_status).collect::<Vec<_>>(),
             vec![UploadStatus::Planned; manifest.uploads.len()]);
  ```

  Name the test `dry_run_reads_metadata_only` so the QA command can select it exactly.

- [x] **1.10 Add dry-run CLI, MCP, and schema contract tests.**

  In `main.rs`, test clap parsing for `deploy-branch staging --repo-root /repo --base origin/dev --dry-run`. Assert `head_ref == "HEAD"` and `verify == true`. In `tools.rs`, test `DeployBranchArgs` deserialization with the same defaults and invalid-parameter mapping. In `schema.rs`, assert that all manifest counts are integer schemas with no unsupported unsigned format.

  Prefix these test names with `deploy_branch_contract` so the Slice 1 QA command selects all of them.

- [x] **1.11 Implement the shared dry-run CLI and MCP path.**

  Add the CLI arguments:

  ```rust
  DeployBranch {
      profile: String,
      #[arg(long)] repo_root: String,
      #[arg(long = "base")] base_ref: String,
      #[arg(long = "head", default_value = "HEAD")] head_ref: String,
      #[arg(long = "no-verify", action = clap::ArgAction::SetFalse, default_value_t = true)] verify: bool,
      #[arg(long)] dry_run: bool,
  }
  ```

  Add MCP `DeployBranchArgs` with serde default functions for `HEAD` and `true`. Both wrappers load the profile, build one `DeployBranchRequest`, call `plan_branch`, and return `dry_run_manifest` when `dry_run` is true. Keep the actual-execution branch private to the orchestration function that Task 2 completes. Invalid roots and refs map to MCP invalid parameters. CLI serializes the manifest with `serde_json::to_writer_pretty`.

- [x] **1.12 Run the Slice 1 QA commands.**

  Run every command under `qa.md#slice-1-qa-deterministic-plan-and-dry-run`. Save a non-empty report with `Status: PASS` only after every command exits zero.

- [x] **1.13 Update the README for the verified dry-run behavior.**

  Document the command and MCP inputs, exact committed-blob planning, dirty-state reporting, full-range union, and deleted-path reporting. State that reported deletions do not remove remote files.

- [x] **1.14 Commit the Slice 1 implementation.**

  Run `git diff --check`, stage only Slice 1 files and its checked `plan.md` micro-steps, then commit:

  ```sh
  git commit -m "feat: plan worktree branch deployments"
  ```

---

## Task 2: Binary upload and in-operation verification

**GDD slice:** 2

**Behavior references:** S2.1 through S2.6 in `scenarios.md`; requirements "Actual deployment uses one binary FTP session" through "Deployment returns a complete structured manifest" in the OpenSpec capability spec.

**QA reference:** `qa.md#slice-2-qa-binary-upload-and-verification`

**Files:**

- Create: `mcp/src/branch_deploy/execute.rs`
- Modify: `mcp/src/branch_deploy/mod.rs`
- Modify: `mcp/src/branch_deploy/git.rs`
- Modify: `mcp/src/branch_deploy/tests.rs`
- Modify: `mcp/src/ftp.rs`
- Modify: `mcp/src/main.rs`
- Modify: `mcp/src/tools.rs`
- Modify: `README.md`

**Consumes:** `BranchDeployPlan`, manifest statuses, exact blob IDs, `config::Profile`, and `suppaftp::{FtpError, FileType}`.

**Produces:**

```rust
pub trait BranchRemote {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure>;
    fn mkdir_p(&mut self, path: &str) -> Result<(), RemoteFailure>;
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<u64, RemoteFailure>;
    fn compare_remote_bytes(
        &mut self,
        path: &str,
        expected: &[u8],
    ) -> Result<RemoteComparison, RemoteFailure>;
    fn delete_file(&mut self, path: &str) -> Result<(), RemoteFailure>;
}

pub struct RemoteComparison { pub matches: bool, pub bytes_read: u64 }
pub enum RemoteFailureKind { Operation, ConnectionLost }

pub trait BlobSource {
    fn read_blob(&mut self, object_id: &str) -> Result<Vec<u8>, BranchDeployError>;
}

pub fn execute_deploy<R: BranchRemote, B: BlobSource>(
    plan: BranchDeployPlan,
    verify: bool,
    blobs: &mut B,
    remote: &mut R,
) -> BranchDeployManifest;
```

- [x] **2.1 Add the in-memory remote, blob source, and failing success-path tests.**

  Implement test fakes that record an enum call log:

  ```rust
  enum RemoteCall {
      Binary,
      Mkdir(String),
      Upload(String, Vec<u8>),
      Compare(String, Vec<u8>),
      Delete(String),
  }
  ```

  Add tests prefixed `executor_` for binary mode first, deterministic upload order, one blob read at a time, parent directory creation, exact uploaded bytes, immediate comparison, matching byte counts, and verification disabled with no `Compare` call.

- [x] **2.2 Run the executor tests and capture the expected red state.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp branch_deploy::tests::executor -- --nocapture
  ```

  Expected result: compilation fails because `execute.rs`, `BranchRemote`, and `BlobSource` do not exist.

- [x] **2.3 Add the batch blob reader and minimal successful executor.**

  `BatchBlobReader` starts one `git -C <root> cat-file --batch` child with piped stdin and stdout. For each requested object ID, write one line, parse `<oid> blob <size>`, read exactly `size` bytes plus the terminating newline, and return the bytes. Reject a missing object, non-blob type, malformed header, short read, or unexpected trailer as `BranchDeployError::Other`.

  In `execute_deploy`, call `set_binary_mode` once. For each upload, read one blob, create its parent, upload it, compare it immediately when verification is enabled, update that entry, and drop the byte vector before moving to the next upload.

- [x] **2.4 Run the successful executor tests until they pass.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp branch_deploy::tests::executor -- --nocapture
  ```

  Expected result: success-path call order and manifest counts pass.

- [x] **2.5 Add failing failure-state tests.**

  Let each fake operation inject either failure kind at one call index. Add exact tests:

  ```rust
  #[test] fn executor_operation_upload_failure_continues() {}
  #[test] fn executor_operation_compare_failure_continues() {}
  #[test] fn executor_verification_mismatch_drains_and_fails_manifest() {}
  #[test] fn executor_connection_loss_marks_remaining_not_attempted() {}
  #[test] fn executor_never_reconnects_after_connection_loss() {}
  ```

  Assert the full ordered `uploads`, `failures`, and `counts`, not only `success`.

- [x] **2.6 Implement deterministic failure transitions.**

  Map an `Operation` failure to the current entry and continue. Map `ConnectionLost` to the current entry, mark every later upload and verification `not_attempted`, and stop without constructing another remote. Treat mismatch as a verification failure after `compare_remote_bytes` returns its complete byte count. Compute `success` only after all statuses settle.

- [x] **2.7 Add failing typed FTP adapter tests.**

  In `ftp.rs`, add tests prefixed `branch_adapter_` for:

  ```rust
  FtpError::ConnectionError(_) => RemoteFailureKind::ConnectionLost
  FtpError::SecureError(_) => RemoteFailureKind::ConnectionLost
  FtpError::UnexpectedResponse(_) => RemoteFailureKind::Operation
  FtpError::BadResponse => RemoteFailureKind::ConnectionLost
  FtpError::InvalidAddress(_) => RemoteFailureKind::Operation
  FtpError::DataConnectionAlreadyOpen => RemoteFailureKind::ConnectionLost
  ```

  Add a reader test where the first byte mismatches but later reads increment a shared counter. Assert comparison returns `matches=false` and the counter equals the full remote length.

- [x] **2.8 Implement typed branch operations on `FtpClient`.**

  Preserve every existing method. Add branch-specific internal helpers that return `FtpError` before mapping to `RemoteFailure`. Implement binary selection with `transfer_type(FileType::Binary)`. Implement comparison with `retr` and a fixed buffer:

  ```rust
  let mut offset = 0usize;
  let mut matches = true;
  let mut bytes_read = 0u64;
  loop {
      let read = reader.read(&mut buffer).map_err(FtpError::ConnectionError)?;
      if read == 0 { break; }
      bytes_read += read as u64;
      let end = offset.saturating_add(read);
      if end > expected.len() || expected[offset..end] != buffer[..read] {
          matches = false;
      }
      offset = end;
  }
  matches &= offset == expected.len();
  ```

  The closure must return normally after EOF so `suppaftp` finalizes RETR. Implement `BranchRemote for FtpClient` with the approved five methods.

- [x] **2.9 Add the opt-in pinned disposable FTP test.**

  Add an ignored test named `disposable_branch_round_trip`. Guard it with `ZED_FTP_RUN_FTP_INTEGRATION=1`. Launch the pinned image:

  ```text
  delfer/alpine-ftp-server:latest@sha256:60bb774d8408d9d4d5c74d05d1c086a34ce192c6c1a142ffac268cac0dbc6fac
  ```

  Use one dynamically mapped control port and one host-selected passive port mapped to the same container port. Set `USERS=test|test|/home/test`, `ADDRESS=127.0.0.1`, and the same `MIN_PORT` and `MAX_PORT`. Put a small TCP control proxy between the client and container. Record client commands and assert `TYPE I` occurs before `STOR` and `RETR`. Upload bytes containing NUL, CRLF, `0xff`, and `0x80`, then compare them through the adapter. A Drop guard must run `docker rm -f <test-container-id>` for the container created by that test.

- [x] **2.10 Complete actual CLI and MCP orchestration.**

  For `dry_run=false`, plan first, then create one `BatchBlobReader` and one `FtpClient`. Credential access stays inside `FtpClient::connect`. Run `execute_deploy`, call best-effort `quit`, and return the manifest. MCP returns the manifest even when `success=false`. CLI always prints it and returns a nonzero process exit when `success=false`.

  Prefix interface tests with `deploy_branch_execution_contract`. Inject fakes below the public wrapper so tests do not read the OS keychain or contact a network.

- [x] **2.11 Run the Slice 2 QA commands.**

  Run every command under `qa.md#slice-2-qa-binary-upload-and-verification`, including the ignored disposable FTP test. Save a non-empty report with `Status: PASS` only after every command exits zero.

- [x] **2.12 Update the README for actual deployment and verification.**

  Document default verification, `--no-verify`, binary transfer, one-session behavior, mismatch and failure manifest semantics, and nonzero CLI exit. Do not instruct an agent to download files manually.

- [x] **2.13 Commit the Slice 2 implementation.**

  Run `git diff --check`, stage only Slice 2 files and its checked `plan.md` micro-steps, then commit:

  ```sh
  git commit -m "feat: verify worktree branch uploads"
  ```

---

## Task 3: Explicit pinned branch-file deletion

**GDD slice:** 3

**Behavior references:** S3.1 through S3.7 in `scenarios.md`; requirements "Remote deletion requires a separate explicit invocation" through "Existing FTP operations remain compatible" in the OpenSpec capability spec.

**QA reference:** `qa.md#slice-3-qa-explicit-pinned-deletion`

**Files:**

- Modify: `mcp/src/branch_deploy/mod.rs`
- Modify: `mcp/src/branch_deploy/git.rs`
- Modify: `mcp/src/branch_deploy/execute.rs`
- Modify: `mcp/src/branch_deploy/tests.rs`
- Modify: `mcp/src/ftp.rs`
- Modify: `mcp/src/main.rs`
- Modify: `mcp/src/tools.rs`
- Modify: `mcp/src/schema.rs`
- Modify: `README.md`

**Consumes:** exact Git planner semantics, `BranchRemote::delete_file`, typed remote failures, remote path mapping, and shared manifest failure records.

**Produces:**

```rust
pub struct DeleteBranchFilesRequest {
    pub profile: String,
    pub repo_root: String,
    pub base_commit: String,
    pub head_commit: String,
    pub paths: Vec<String>,
    pub reason: String,
    pub dry_run: bool,
}

pub struct BranchDeletePlan {
    pub profile: String,
    pub repository_root: String,
    pub base_commit: String,
    pub head_commit: String,
    pub reason: String,
    pub paths: Vec<DeletePathResult>,
    pub blocked: Vec<BlockedPath>,
}

pub fn plan_deletion(
    request: &DeleteBranchFilesRequest,
    profile: &Profile,
) -> Result<BranchDeletePlan, BranchDeployError>;

pub fn execute_deletion<R: BranchRemote>(
    plan: BranchDeletePlan,
    remote: &mut R,
) -> BranchDeleteManifest;
```

- [ ] **3.1 Add failing pinned-authorization tests.**

  Add tests prefixed `deletion_preflight_` for empty reason, empty path list, abbreviated commit IDs, unresolved IDs, exact full IDs, a path outside the recomputed deleted set, duplicate exact paths, and deterministic order. Assert all invalid requests stop before credential and remote counters increment.

- [ ] **3.2 Add failing atomic path-safety tests.**

  Cover one safe plus one unsafe path, non-UTF-8 Git deletion, every non-ASCII character class, two requested paths equal under `eq_ignore_ascii_case`, and a case-only rename where the requested old path collides with a surviving head blob. Assert every blocked path and reason appears and no path is authorized when any blocker exists.

- [ ] **3.3 Implement deletion planning as pinned-set authorization.**

  Require both commit inputs to be 40 lowercase hexadecimal characters. Resolve each with `rev-parse --verify <id>^{commit}` and require the canonical output to equal the input exactly. Recompute the full-range touched set and head tree. Define the authorized deleted set as touched paths absent from the head tree.

  Validate every requested path before returning. Reject empty reason after trimming. Reject empty paths, duplicates, paths outside the deleted set, lexical failures, non-UTF-8, and every non-ASCII byte. Build one ASCII-folded index containing all requested paths and all surviving head blobs. Record collisions from that index. If `blocked` is non-empty, return the complete rejected deletion manifest without keychain or remote access.

- [ ] **3.4 Run deletion preflight tests until they pass.**

  Run:

  ```sh
  cargo test -p zed-ftp-mcp branch_deploy::tests::deletion_preflight -- --nocapture
  ```

  Expected result: all authorization and atomic rejection cases pass.

- [ ] **3.5 Add failing deletion executor tests.**

  Add tests prefixed `deletion_executor_` for dry-run planned statuses, binary mode before delete, exact path order, an `Operation` failure followed by a successful delete, and `ConnectionLost` followed by remaining `not_attempted` paths. Assert no upload or comparison call appears in the fake log.

- [ ] **3.6 Implement deletion execution.**

  Dry run returns planned statuses without constructing a remote. Actual execution selects binary mode once and calls `delete_file` for exact preflight-approved remote paths. Continue after `Operation`. Stop after `ConnectionLost`, do not reconnect, and mark later paths `not_attempted`. Compute manifest counts and `success` from final statuses.

- [ ] **3.7 Add deletion CLI, MCP, and schema contract tests.**

  Parse this CLI shape:

  ```text
  delete-branch-files <profile> --repo-root <absolute-path> --base-commit <id> --head-commit <id> --path <git-path>... --reason <text> [--dry-run]
  ```

  Add MCP `ftp_delete_branch_files` with the same required values. Test all-preflight rejection as a structured manifest, successful dry run, unsuccessful execution manifest, and CLI nonzero exit. Extend schema checks to every deletion count.

- [ ] **3.8 Implement the separate deletion interfaces.**

  Both wrappers call `plan_deletion` before any connector. If preflight rejects, return its structured unsuccessful manifest. If dry run succeeds, return planned statuses. Otherwise create one `FtpClient`, execute, best-effort quit, and return the full result. Do not call this path from `deploy_branch`.

- [ ] **3.9 Add the disposable deletion integration test.**

  Reuse the Task 2 container guard. Seed two ASCII files through the test adapter. Invoke deletion for one exact path and assert that RETR fails for only that path while the unrequested file still returns its original bytes. Name the ignored test `disposable_branch_deletion`. Remove only the test-created container and its temporary data.

- [ ] **3.10 Complete README documentation.**

  Add both interfaces, pinned commit requirements, exact path list, required reason, atomic blockers, ASCII-only limit, case-collision behavior, dry run, and failure statuses. State that an agent must explain a deletion and receive user approval before making the separate explicit call.

- [ ] **3.11 Run compatibility and release gates.**

  Run every command under `qa.md#slice-3-qa-explicit-pinned-deletion`. Also compare existing tool schemas before and after the change and run their current tests. Treat any warning, failure, or flake as blocking.

- [ ] **3.12 Commit the Slice 3 implementation.**

  Run `git diff --check`, stage only Slice 3 files and its checked `plan.md` micro-steps, then commit:

  ```sh
  git commit -m "feat: require explicit branch file deletion"
  ```

## Feature closing sequence

After all three slice verification gates are `[x] VERIFIED`, dispatch one fresh Branch Reviewer against the full OpenSpec spec and complete commit range. Replay any affected slice after a fix. Then run OpenSpec Verify, the retrospective, OpenSpec archive, and `superpowers:finishing-a-development-branch` in that order.
