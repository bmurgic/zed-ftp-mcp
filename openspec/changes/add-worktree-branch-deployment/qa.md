# Branch deployment QA procedures

Run each procedure against the current Hardener-approved revision. Save the complete command output in the slice QA report. Do not use a saved FTP profile, an OS keychain entry, or a real FTP server.

## Slice 1 QA: deterministic plan and dry run

**Behavior references:** S1.1 through S1.8 in `scenarios.md`.

1. Run the planner and path-safety suite.

   ```sh
   cargo test -p zed-ftp-mcp branch_deploy::tests::planner -- --nocapture
   ```

   Expected result: every fixture passes, including the linked-worktree root, restored path, merge, dirty worktree, deleted path, submodule, and unsafe path cases.

2. Run the dry-run side-effect discrimination test.

   ```sh
   cargo test -p zed-ftp-mcp branch_deploy::tests::dry_run_reads_metadata_only -- --exact --nocapture
   ```

   Expected result: the test proves zero blob-content reads, credential reads, remote factory calls, and remote method calls.

3. Run the CLI, MCP, and schema contract tests.

   ```sh
   cargo test -p zed-ftp-mcp deploy_branch_contract -- --nocapture
   ```

   Expected result: defaults are `head_ref=HEAD` and `verify=true`; dry run returns the complete manifest shape; invalid roots and refs map to invalid parameters.

4. Run the Slice 1 final suite.

   ```sh
   cargo test -p zed-ftp-mcp branch_deploy -- --nocapture
   ```

   Report `VERIFIED` only if every command passes and the captured assertions cover S1.1 through S1.8.

## Slice 2 QA: binary upload and verification

**Behavior references:** S2.1 through S2.6 in `scenarios.md`.

1. Run the in-memory executor suite.

   ```sh
   cargo test -p zed-ftp-mcp branch_deploy::tests::executor -- --nocapture
   ```

   Expected result: call logs prove binary mode first, exact path order, immediate comparison, no comparison when disabled, Operation continuation, and ConnectionLost cutoff without reconnect.

2. Run the typed FTP adapter suite.

   ```sh
   cargo test -p zed-ftp-mcp ftp::tests::branch_adapter -- --nocapture
   ```

   Expected result: complete-stream comparison, byte counts, mismatch drain, and typed error classification pass for plain and TLS stream dispatch.

3. Run the disposable FTP integration test. Docker must be available. The test uses `delfer/alpine-ftp-server:latest@sha256:60bb774d8408d9d4d5c74d05d1c086a34ce192c6c1a142ffac268cac0dbc6fac`, creates an in-memory profile and password, and removes its container and data when the test ends.

   ```sh
   ZED_FTP_RUN_FTP_INTEGRATION=1 cargo test -p zed-ftp-mcp ftp::tests::disposable_branch_round_trip -- --ignored --exact --nocapture
   ```

   Expected result: the captured control commands contain `TYPE I` before `STOR` and `RETR`; arbitrary binary bytes upload and compare exactly; one control session is used; the container is removed.

4. Run the CLI and MCP unsuccessful-manifest tests.

   ```sh
   cargo test -p zed-ftp-mcp deploy_branch_execution_contract -- --nocapture
   ```

   Expected result: MCP returns an unsuccessful manifest as data; CLI emits the same manifest semantics and exits nonzero.

5. Run the Slice 2 final suite.

   ```sh
   cargo test -p zed-ftp-mcp -- --nocapture
   ```

   Report `VERIFIED` only if the local suites and the disposable FTP integration test pass against the same revision.

## Slice 3 QA: explicit pinned deletion

**Behavior references:** S3.1 through S3.7 in `scenarios.md`.

1. Run deletion authorization and preflight tests.

   ```sh
   cargo test -p zed-ftp-mcp branch_deploy::tests::deletion_preflight -- --nocapture
   ```

   Expected result: incomplete commit IDs, paths outside the recomputed set, unsafe paths, non-ASCII paths, and ASCII case collisions reject the whole call before credential or remote access.

2. Run deletion executor tests.

   ```sh
   cargo test -p zed-ftp-mcp branch_deploy::tests::deletion_executor -- --nocapture
   ```

   Expected result: exact authorized paths run in deterministic order; Operation failures continue; ConnectionLost stops; dry run performs no remote call.

3. Run the disposable FTP deletion integration test.

   ```sh
   ZED_FTP_RUN_FTP_INTEGRATION=1 cargo test -p zed-ftp-mcp ftp::tests::disposable_branch_deletion -- --ignored --exact --nocapture
   ```

   Expected result: only the explicitly requested seeded file is absent after the call; an unrequested seeded file remains; the test removes the container and data.

4. Run the full release gate.

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace
   cargo check -p zed-ftp --target wasm32-wasip1
   openspec validate add-worktree-branch-deployment --strict
   ```

   Expected result: every command exits zero with no warning or flaky retry.

5. Inspect the README examples against the CLI help and MCP schemas.

   ```sh
   cargo run -p zed-ftp-mcp -- deploy-branch --help
   cargo run -p zed-ftp-mcp -- delete-branch-files --help
   cargo test -p zed-ftp-mcp schema::tests::mcp_output_schemas_use_compatible_unsigned_integers -- --exact
   ```

   Expected result: names, defaults, required deletion reason, exact path arguments, and manifest fields match the documentation.

   Report `VERIFIED` only if all procedures pass against the same Hardener-approved revision.
