# Retrospective: merge-aware-deploy

## Outcome

The three slices and the closing roles are complete. The Gauntlet-Driven Development (GDD) workflow engine reports `Workflow status: COMPLETE`.

- The feature Hardener needed two passes. The first added tests that killed the surviving mutants and returned `REVERIFY_REQUIRED`. The second confirmed the result with no new findings.
- Feature QA ran all six `qa.md` procedures against a disposable FTP server, with 97 checks and 0 failures.
- The Branch Reviewer blocked on GDD-F0016. The documented conflict loop could not terminate. Brandon chose the exit path: commit the resolved file, upload it with `ftp_upload_file`, then merge again.
- The Security Reviewer found GDD-F0025. `ftp_deploy_commits` passed caller-supplied commit values to `git diff-tree` as options.
- One closing fixer round fixed GDD-F0016 to GDD-F0026. Both scoped re-reviews verdicted every finding ADDRESSED.
- The re-review raised two new Minor findings, GDD-F0027 and GDD-F0028. The advisor dismissed both under case C4. The three exception clauses for GDD-F0028 landed before archive.

## Notes

- GDD-F0015 shows as deferred in the digest, but the fix landed as GDD-F0020 in ca7ce82. The engine does not allow a Low finding to move from `REPORTED` to `RESOLVED`.
- GDD-F0012 is the git environment isolation in `drift.rs`. It is partly addressed: commit 4ae5704 added the shared `git_process` module, which keeps two ways to start Git on purpose.
- GDD-F0026 was limited to the tools this branch changed. These handlers still need the same path check: `ftp_download_file`, `ftp_delete_file`, `ftp_list`, `ftp_mkdir`, and `ftp_delete_dir`.
- The fix for GDD-F0025 uses `--end-of-options`, which needs git 2.24 or newer.

## Role yield

role	dispatches	minutes	tokens	findings	real	false
implementer	3	60	280697	0	0	0
slice-reviewer	3	4	-	0	0	0
refiner	3	77	-	11	0	0
slice-qa	3	64	-	1	0	0
hardener	2	130	-	3	0	0
feature-qa	1	6	-	0	0	0
branch-reviewer	3	35	-	11	9	2
security-reviewer	2	16	-	2	2	0
fixer	1	20	-	-	-	-

## Findings left unchanged

Finding ID: GDD-F0001
Origin: slice-1-refiner
Ruling: slice-1-refiner; disposable FTP tests hang at random because wait_for_ftp checks only TCP connect and start_control_proxy never closes the client socket; mcp/src/ftp.rs:1236,1279
Evidence digest: 5bb78e7838496348d79bdbeb9c4a2d6cbfe3c5f46541ab2ea3d7b141d9dc9f06

Finding ID: GDD-F0002
Origin: slice-1-refiner
Ruling: slice-1-refiner; six pre-existing functions the slice touched stay above CRAP 6 (plan_branch, plan_deletion, commit_tree, BatchBlobReader::new, main, FtpServer::ftp_deploy_branch); mcp/src/branch_deploy/git.rs:40,128,218,501
Evidence digest: 9d05e71a6a680cceeed0a0ea83c3a2ace399666db86f3087de9323485eb028b8

Finding ID: GDD-F0003
Origin: slice-1-refiner
Ruling: slice-1-refiner; a merge adding a file under a new directory blocks with download_failed on servers that error on NLST of a missing parent, per spec line 205; mcp/src/ftp.rs:105,121
Evidence digest: d5e9bce1c8b421f5550e056383da04b6c91bbf2690c75d3caaf526bb8da3a128

Finding ID: GDD-F0004
Origin: slice-1-qa
Ruling: slice-1-qa; pre-existing: branch mkdir fails on vsftpd because its 550 for an existing directory lacks 'already exists'; reproduces at 5dbfa22; mcp/src/ftp.rs:344-347
Evidence digest: 31e3e1ef68fa308dd28c211a168cc9896247fb309be0747ebff2f8822b6aaafe

Finding ID: GDD-F0005
Origin: slice-2-refiner
Ruling: slice-2-refiner; Low, pre-existing: branch_deploy names crate::ftp::FtpClient in connector signatures, so connect_and_run success path has no unit test; mcp/src/branch_deploy/mod.rs:406-501
Cost if wrong: an untested connection-success path hides a regression in connect_and_run until QA
Wake condition: a change touches the branch_deploy connector or the Branch Reviewer rules it in scope
Evidence digest: bbed11d8579b473b9790f36e0bcb0b359b2d9ed7dee25a002445e0be0014af40

Finding ID: GDD-F0006
Origin: slice-2-refiner
Ruling: slice-2-refiner; Low, pre-existing: DeployMode derives clap::ValueEnum so core depends on CLI framework; mcp/src/branch_deploy/mod.rs:22
Cost if wrong: core module stays coupled to clap; a later CLI change forces a core edit
Wake condition: a change touches DeployMode or the CLI mode flag, or the Branch Reviewer rules it in scope
Evidence digest: fd9a54f447534cdd985c7eddee28935bab522308edf37f8b7574efa8e686b07a

Finding ID: GDD-F0007
Origin: slice-3-refiner
Ruling: slice-3-refiner; Low, pre-existing: five changed functions above CRAP 6 (deploy_commits, changed_paths_for_commit, three tool handlers); mcp/src/deploy.rs, mcp/src/tools.rs
Cost if wrong: harder review of later edits
Wake condition: a change edits these functions or Branch Reviewer rules it in scope
Evidence digest: 9ff0512b4bd1a20a9b33f54e7d73f3710cc5d6ad626d594d8681a489984f550b

Finding ID: GDD-F0008
Origin: slice-3-refiner
Ruling: slice-3-refiner; Low: import cycle widened to branch_deploy-deploy-drift-ftp via RemoteFailure; mcp/src/drift.rs
Cost if wrong: coupling grows with later changes
Wake condition: GDD-F0005 is repaired or Branch Reviewer rules it in scope
Evidence digest: d1129c9eac2f8a8b769bbe10948db970c704c5ddcda9980399d10f9cd240be4a

Finding ID: GDD-F0009
Origin: slice-3-refiner
Ruling: slice-3-refiner; Medium, pre-existing: deploy_commits uploads nothing when local_root is a repo subdirectory (diff-tree paths are repo-relative); already queued as a separate task; mcp/src/deploy.rs
Cost if wrong: commit deploys from subfolder profiles silently do nothing, including with expect_ref
Wake condition: the queued subfolder-bug task runs or Branch Reviewer rules it in scope
Evidence digest: 827a73e478d4c4703071f8476beefd0acb0e38facd71592053e165e2367345c5

Finding ID: GDD-F0010
Origin: slice-3-refiner
Ruling: slice-3-refiner; Low: changed_paths_for_commit parses git output by line instead of NUL-separated; mcp/src/deploy.rs
Cost if wrong: paths with newlines mis-split
Wake condition: Branch Reviewer rules it in scope
Evidence digest: fb0c22c646121b4cd37eb3958bc32de4cde9c55b6fe836cc44f3e62af88076f7

Finding ID: GDD-F0011
Origin: slice-3-refiner
Ruling: slice-3-refiner; Low, needs spec ruling: drift check sets TYPE I on the shared connection so guarded uploads run in binary while unguarded use server default; mcp/src/drift.rs
Cost if wrong: a line-ending-translating server stores different bytes with and without expect_ref
Wake condition: Branch Reviewer or Brandon rules on transfer mode
Evidence digest: 7eb8de7a6af605b775b28361293e0c81532e6df91454f31b586a8ab305cc6e28

Finding ID: GDD-F0012
Origin: slice-3-refiner
Ruling: slice-3-refiner; Low: drift::run_git does not isolate git environment like configure_git_command; mcp/src/drift.rs
Cost if wrong: user git env vars change drift results
Wake condition: Branch Reviewer rules it in scope
Evidence digest: 95c4b6017ddc613b4b59219a58fd1a4dd026d0b0ccb63a75353294b05bd1fa34

Finding ID: GDD-F0013
Origin: feature-hardener
Ruling: feature-hardener; Low: CRAP above 6 in 11 functions; duplicates F0002 and F0007
Cost if wrong: complex handlers stay under-tested
Wake condition: Branch Reviewer rules it in scope
Evidence digest: 3cb1bb8c85aa41aa8a3017a1b43fc052f1816663ecdd6a8fc2802e86793882b8

Finding ID: GDD-F0014
Origin: feature-hardener
Ruling: feature-hardener; Low: merge blocking causes (connect, binary mode, upload loss, tool error) lack scenarios; mcp/src/branch_deploy/execute.rs, merge.rs
Cost if wrong: a regression in a blocking path ships unnoticed
Wake condition: Branch Reviewer rules it in scope
Evidence digest: a61bf0cd3a4f7777046a54f18e1fb89f6a75aee388c2f8a21471459c943883d8

Finding ID: GDD-F0015
Origin: feature-hardener
Ruling: FIXED by GDD-F0020 in ca7ce82 (Branch Reviewer FIX ruling); include_str! tests deleted; engine refuses REPORTED -> RESOLVED for Low
Cost if wrong: none; the tests no longer exist at HEAD
Wake condition: an include_str! source-text test reappears
Evidence digest: b60b0cc0bea70e310ccc90f40ff30e9c24abc5d7ab12d9d23fff889ce0818437

Finding ID: GDD-F0027
Origin: feature-branch-review
Ruling: README conflict exit path uploads without TYPE I; fails safe by re-conflicting; code fix (TYPE I in FtpClient::connect) is a follow-up
Cost if wrong: on servers that rewrite ASCII line endings the documented conflict exit re-conflicts and never terminates
Wake condition: the follow-up task that sets TYPE I in FtpClient::connect
Evidence digest: 16d26118fc924abcb5924d740b613e54395d587b2942af5d123c88669395541c

Finding ID: GDD-F0028
Origin: feature-branch-review
Ruling: Brandon-approved F0026 path check changes the no-expect_ref default; add one exception clause each to spec.md, proposal.md, design.md at archive/sync
Cost if wrong: main spec promises a frozen default the code no longer honors
Wake condition: the archive or sync step
Evidence digest: 06b7529d560914d729c57e3cdd018778229fde0bed9932f64d0ff906d2ff5003


## Branch Reviewer LEAVE rulings

- GDD-F0001 | LEAVE | disposable-test hang is a Docker/harness condition, not product code; feature QA ran all 7 ignored tests to completion.
- GDD-F0002 | LEAVE | CRAP > 6 on the slice-1 executor was already split into named steps (8899ead, b6a7a90); the remaining complexity is the decision table itself and is covered by merge_rules_01.
- GDD-F0003 | LEAVE | the NLST-of-missing-parent -> download_failed behavior is what spec.md:205 requires; changing it is a spec change, not a fix. Cite together with finding 2, which shows the same NLST premise fails the other way for dotfiles; a spec revision should address both.
- GDD-F0004 | LEAVE | vsftpd "550 Create directory operation failed" on an existing directory is pre-existing at base c6a0ba9 (that commit matches only the "already exists" phrase); out of this change's scope.
- GDD-F0005 | LEAVE | `branch_deploy` naming `FtpClient` in adapter code is cosmetic; the executor itself stays behind `BranchRemote`.
- GDD-F0006 | LEAVE | `DeployMode` deriving `clap::ValueEnum` couples a shared type to the CLI crate, but a separate CLI enum would duplicate the two variants and the conversion; no behavior impact.
- GDD-F0007 | LEAVE | same basis as F0002 for slice 2's routing code; `deploy_or_preview` is small after 508d184.
- GDD-F0008 | LEAVE | drift.rs importing `branch_deploy::RemoteFailure` compiles and is a symptom of finding 4's missing shared layer; fix it there, not separately.
- GDD-F0009 | LEAVE | `deploy_commits` uploading nothing with a subdirectory `local_root` is pre-existing and already queued as its own task (design.md:103).
- GDD-F0010 | LEAVE | `changed_paths_for_commit` parsing `diff-tree` by line is pre-existing code; design.md:90 binds new code, and this branch added no line-parsed Git path output.
- GDD-F0011 | LEAVE | guarded uploads selecting TYPE I is spec-required; unguarded paths not setting a type is pre-existing. Follow-up suggested: TYPE I in `FtpClient::connect`.
- GDD-F0012 | LEAVE | `drift::run_git` env isolation is the Security Reviewer's domain; finding 4's shared `run_git` would close it as a side effect if the controller wants that here.
- GDD-F0013 | LEAVE | same basis as F0002/F0007 for slice 3; drift.rs was split into named steps in 739af0b.
- GDD-F0014 | LEAVE | the extra blocking causes (binary-mode failure, blob read failure, merge tool error) are tested (tests.rs:2786, 2847, 2883) even though gherkin.md has no scenario for them; a Gherkin addition is a spec-authoring task, and finding 7 asks the README to name them.
