Every slice obeys design.md `## Global constraints`. Every scenario runs with in-memory fakes of `BranchRemote`, `BlobSource`, or `DriftRemote` wherever the scenario does not need a real server. Each FTP-adapter behavior (550 plus listing, binary mode) also gets a disposable Docker FTP test, following the existing ignored `ftp::tests::disposable_*` pattern.

## 1. Merge a branch into a server
**Slice state:** [x] VERIFIED
**Depends on:** none
**Files:** mcp/src/branch_deploy/mod.rs, mcp/src/branch_deploy/merge.rs, mcp/src/branch_deploy/execute.rs, mcp/src/branch_deploy/git.rs, mcp/src/branch_deploy/tests.rs, mcp/src/ftp.rs, mcp/src/tools.rs, mcp/src/main.rs, mcp/src/schema.rs, README.md
**Interfaces:** Consumes: none / Produces: `DeployMode` (enum `Overwrite` | `Merge`, serde `overwrite` | `merge`, default `Overwrite`), `DeployBranchRequest.mode: DeployMode`, `PlannedUpload.base_object_id: Option<String>`, `merge::decide(base: Option<&[u8]>, head: &[u8], server: Option<&[u8]>) -> Result<MergeDecision, BranchDeployError>`, `BranchRemote::download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure>`, `FtpClient::download_or_missing(&mut self, path: &str) -> Result<Option<Vec<u8>>, FtpError>`, `execute::decide_merge<R: BranchRemote, B: BlobSource>(plan: &BranchDeployPlan, blobs: &mut B, remote: &mut R) -> MergePhase`
**Gherkin scenarios:** Deploy the default head, Reject a directory that is not the selected worktree root, Reject an unresolved ref, Branch range 04 - mode defaults to overwrite, Branch range 05 - reject an unknown mode, Working tree differs from the head commit, Touched path survives at head, Touched entry is not a deployable blob, Unsafe Git path, Head blobs 05 - merge mode ignores working-tree changes, Uploaded bytes match, Uploaded bytes differ, Verification is disabled, Deployment succeeds, Execution completes unsuccessfully through MCP, Execution completes unsuccessfully through the CLI, Manifest 04 - blocked merge is unsuccessful through the CLI, Manifest 05 - long conflict text is truncated, Manifest 06 - non-UTF-8 conflict text is readable, Manifest 07 - merged upload reports head blob and uploaded size, Compatibility 02 - branch deployment without mode, Merge rules 01 - decision table, Merge rules 02 - conflict reason is reported, Merge rules 03 - server-only line is preserved, Merge rules 04 - adjacent edits conflict, Merge rules 05 - download failure is not treated as missing, Merge rules 06 - a 550 for an absent file means missing, Merge blocking 01 - one conflict blocks clean files, Merge blocking 02 - connection lost while downloading, Merge blocking 03 - everything resolves, Merge blocking 04 - planning failure blocks merge uploads
**QA procedures:** QA procedure: Merge a branch into a server that has its own edits, QA procedure: A conflicting file blocks the whole merge, QA procedure: Overwrite deploys behave as before
**Design:** D1, D2, D3, D4, D5, D6, D7, D9

Independent test criteria: against a disposable FTP server, `deploy-branch --mode merge` keeps a server-only line while it deploys a head change. The same command blocks every upload when one file conflicts. Overwrite mode output is unchanged, apart from the `mode` and `blocked_by_conflicts` fields.

- [x] 1.1 Add `DeployMode` to the request, the CLI `--mode` flag, and the MCP `mode` argument, with the default `overwrite`. Add `mode` and `blocked_by_conflicts` to the manifest, and update the existing exact-manifest and schema tests.
- [x] 1.2 Add the pure `merge::decide` engine (spec rules 2 to 8, NUL-anywhere binary test, `git merge-file -p --diff3`), and its exhaustive rule tests.
- [x] 1.3 Read base tree entries into `PlannedUpload.base_object_id` in merge mode only, and decide rule 1 (`unchanged_in_range`) from blob IDs.
- [x] 1.4 Add `FtpClient::download_or_missing` (550 plus `NLST` parent check) and `BranchRemote::download_bytes`. Cover both with adapter tests and a disposable FTP test.
- [x] 1.5 Add `execute::decide_merge` (phase 1), plus the all-or-nothing blocking, failure records, `not_decided`, and `not_needed` statuses. Then run phase 2, which uploads the chosen bytes, verifies them against the chosen bytes, and fills the manifest fields (`merge_status`, `uploaded_from`, `conflict_reason`, lossy and truncated `marked_text`).
- [x] 1.6 Update the `ftp_deploy_branch` and CLI help text and descriptions per D9, and add the README section "Merging into a server".
- [x] 1.V **Slice verification gate**

## 2. Preview a merge without changing the server
**Slice state:** [~] VERIFYING: REVIEW
**Depends on:** 1
**Files:** mcp/src/branch_deploy/mod.rs, mcp/src/branch_deploy/execute.rs, mcp/src/branch_deploy/tests.rs, mcp/src/tools.rs, mcp/src/main.rs, README.md
**Interfaces:** Consumes: `DeployMode` (enum `Overwrite` | `Merge`, serde `overwrite` | `merge`, default `Overwrite`), `execute::decide_merge<R: BranchRemote, B: BlobSource>(plan: &BranchDeployPlan, blobs: &mut B, remote: &mut R) -> MergePhase` / Produces: none
**Gherkin scenarios:** Dry-run plan succeeds, Dry run 02 - merge preview writes nothing remotely, Dry run 03 - merge preview reports a conflict without uploading
**QA procedures:** QA procedure: Preview a merge without changing the server
**Design:** D4, D7, D9

Independent test criteria: `deploy-branch --mode merge --dry-run` reports every file's merge status, including conflicts, over one binary FTP session. The server stays byte-for-byte unchanged. An overwrite dry run with the server stopped still succeeds offline.

- [x] 2.1 Route a merge-mode dry run through the real blob source and connector: select binary mode, run `decide_merge`, and return before phase 2, with would-upload files `planned`. Prove with a recording fake that it makes no upload, mkdir, or delete call, and that an overwrite dry run makes no connector or blob call.
- [x] 2.2 Update the dry-run wording in the CLI help, the MCP argument docs, and the README so they say that a merge preview connects to the server but never writes.
- [ ] 2.V **Slice verification gate**

## 3. Refuse to clobber drifted server files
**Slice state:** [ ] QUEUED
**Depends on:** 1
**Files:** mcp/src/drift.rs, mcp/src/deploy.rs, mcp/src/tools.rs, mcp/src/ftp.rs, mcp/src/schema.rs, README.md
**Interfaces:** Consumes: `FtpClient::download_or_missing(&mut self, path: &str) -> Result<Option<Vec<u8>>, FtpError>` / Produces: `drift::resolve_expect_ref(repo_dir: &Path, expect_ref: &str) -> Result<ResolvedRef, DriftError>`, `drift::check_drift<R: DriftRemote>(remote: &mut R, targets: &[DriftTarget], resolved: &ResolvedRef) -> Result<DriftCheck, DriftError>`
**Gherkin scenarios:** Existing tool is invoked, Expected ref 01 - omitted expected ref keeps overwrite behavior, Expected ref 02 - unresolvable expected ref is rejected, Expected ref 03 - upload source outside a repository, Expected ref 04 - expected path is not a regular file, Expected ref 05 - unreadable local file, Drift classification 01 - classification table, Drift classification 02 - single-file upload of the committed version, Drift classification 03 - download error is not treated as missing, Drift refusal 01 - one drifted file blocks the others, Drift refusal 02 - no drift uploads normally, Drift refusal 03 - single-file upload refused, Drift dry run 01 - drift is reported without uploading, Drift dry run 02 - dry run without expected ref stays offline
**QA procedures:** QA procedure: Deploy a directory without clobbering server changes, QA procedure: Upload one file only when the server matches what you expect
**Design:** D8, D9

Independent test criteria: with `expect_ref` set, `ftp_deploy`, `ftp_deploy_commits`, and `ftp_upload_file` refuse the whole run and name the drifted files, while creating no directory. Without `expect_ref`, all three behave exactly as before.

- [ ] 3.1 Add `drift.rs`: `resolve_expect_ref` (peeled `^{commit}`), expected-copy lookup through `ls-tree` and `cat-file`, `DriftRemote` (binary mode plus `download_bytes` through `download_or_missing`), `check_drift`, and the `DriftCheck` result shape, all tested with a fake remote and temp repositories.
- [ ] 3.2 Wire `expect_ref` into `ftp_deploy` and `ftp_deploy_commits`: target files, a routable invalid-args error, the check before mkdir, the refused counts, and the online dry run. Add `drift_check` to `DeployPlan`.
- [ ] 3.3 Wire `expect_ref` into `ftp_upload_file`, including `before_changes` upload bytes, and add `drift_check` to `UploadResponse`.
- [ ] 3.4 Update the three tool descriptions per D9, and add the `expect_ref` subsection and the autocrlf caveat to the README.
- [ ] 3.V **Slice verification gate**
