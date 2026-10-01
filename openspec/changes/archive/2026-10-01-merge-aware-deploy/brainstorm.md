# Merge-aware deploy — brainstorm record (2026-09-29)

## Background
On 2026-09-28 a bulk upload of tms-falcon `dev` files to staging overwrote `application/models/Mails.php`, reverting the driver-portal 2FA email fix (TravelokoInc/tmsfalcon-php PR #82). Staging's `Mailer.php` also carries a staging-only BCC line not in git. Goal: uploads that merge with, or refuse to clobber, the server's current copy. Existing behavior stays the default.

Upload paths (at c6a0ba9): `ftp_upload_file` (working tree or HEAD), `ftp_deploy` (full local_root walk), `ftp_deploy_commits` (paths from commits, working-tree bytes), `ftp_deploy_branch` (base..head, exact head blobs, optional verify, governed by openspec/specs/branch-deployment/spec.md).

## Decisions
1. Scope. Options: merge in branch only + drift guard on others / merge in all four / branch only. Chosen: three-way merge in `ftp_deploy_branch` only; opt-in drift guard on the other three. Reason: only the branch tool has a real BASE. Brandon also asked for documentation so "merge into staging" is understood by a future agent.
2. Merge dry run. Options: online merge preview / offline only / all dry runs online. Chosen: `dry_run=true` with merge downloads and merges in memory, never uploads; overwrite dry runs stay offline. Amends the spec's offline dry-run requirement.
3. Conflict handling. Options: all-or-nothing / upload clean files, skip conflicts. Chosen: all-or-nothing — any conflict or download failure blocks every upload. Reason: half-merged deploys break interdependent files.
4. Drift guard reference. Options: optional `expect_ref`, no default / plus default for deploy_commits / always-on with `force`. Chosen: optional `expect_ref`, no defaults, no `force`.

## Approved design
### Section 1 — contract
`ftp_deploy_branch` gains `mode: "overwrite" | "merge"` (default overwrite). Merge: plan; read BASE (base_ref), OURS (head blob), THEIRS (server download); decide every file; block all uploads on any conflict/download failure; else upload; verify compares against uploaded bytes. Spec amendments (merge mode only): uploads head blob or merged bytes, labeled per file; verification against uploaded bytes; merge dry run is an online, non-uploading preview. Caveat: `git merge-file` treats adjacent-line edits as conflicts.

### Section 2 — per-file rules (first match wins; binary = contains NUL)
1. server == head → already_deployed, no upload
2. BASE exists, server == BASE → fast_forward, upload head
3. BASE exists, server differs, all text → git merge-file; merged or conflict
4. BASE exists, server differs, any side binary → conflict
5. BASE exists, server missing → conflict
6. BASE missing, server missing → new_file, upload head
7. BASE missing, server exists and differs → conflict
Deleted paths: reported only, as today. Engine: `branch_deploy/merge.rs`, pure function over three byte arrays using temp files + `git merge-file -p`. `BranchRemote` gains `download_bytes -> Option<Vec<u8>>` (None = 550). Manifest: per-upload `merge_status`, `uploaded_from`, conflict `marked_text` (64 KB cap); top-level `mode`, `blocked_by_conflicts`.

### Section 3 — drift guard, docs, tests
Shared `drift.rs`. With `expect_ref`: download each target; clean if server == file at expect_ref (`git -C <local_root> show <ref>:./<rel>`) or == bytes to upload; missing on server is clean only if missing at ref; any drift refuses the whole run and lists files; dry_run+expect_ref checks online without uploading. Docs: tool descriptions (merge trigger phrases), README "Merging into a server", spec amendments. Tests first via fake remote/blob source: clean merge, conflict blocks all, server-only change preserved, all 7 rows, download failure, merge dry run no upload, overwrite dry run no remote calls, drift refuse/pass/missing cases. Verify: cargo test, clippy, build, throwaway local FTP server; never the `staging` profile. Mac reinstall is Brandon's step.

## Route
OpenSpec GDD (Brandon chose 1).

## Capability slices (MVP first, derived at capture; not separately ruled by Brandon)
1. Walking skeleton: `ftp_deploy_branch mode="merge"` real run — download, decide per file (rows 1-7), all-or-nothing block, upload chosen bytes, manifest merge fields.
2. Merge preview and verification: `dry_run=true` + merge is an online non-uploading preview; verify compares against uploaded bytes; conflict `marked_text`.
3. Drift guard: `expect_ref` on `ftp_upload_file`, `ftp_deploy`, `ftp_deploy_commits`, including dry-run check.
4. Agent-facing docs: tool descriptions, README "Merging into a server".
