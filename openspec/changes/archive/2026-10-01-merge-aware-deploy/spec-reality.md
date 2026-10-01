# Spec-reality report

## Round 1

Two `validator-spec-reality` checkers ran, one per delta spec. This file condenses their facts and leads. The full reports are in the planning session transcript. The parent then checked the `git merge-file` claims that the checkers could not test, using real scratch files:
- Edits on adjacent lines produce a conflict (exit 1).
- Edits on non-adjacent lines merge cleanly (exit 0).
- When head equals base and the server copy is edited, the output equals the server copy (exit 0).
- Latin-1 bytes pass through raw, so the marked text is not valid UTF-8.

### Checker: branch-deployment

| Lead | Severity | Summary | Adjudication |
| --- | --- | --- | --- |
| B1 | Major | FTP 550 on RETR also means permission denied or not a plain file. Treating every 550 as missing can turn an unreadable file into `new_file` (an overwrite) or `deleted_on_server`. | Upheld. Ruling R1. |
| B2 | Major | No rule for head blob == base blob (a path touched but not net-changed). A binary file blocks the run, and a text file is re-uploaded as `merged`. | Upheld and verified. Ruling R2. |
| B3 | Major | `merge_status` has no member for files left undecided (connection lost, connect or binary-mode failure, blob read failure). `blocked_by_conflicts` definition disagrees with the blocking rule. | Upheld. Ruling R3. |
| B4 | Major | `upload_status` and `verification_status` are undefined for conflict files, `already_deployed` files with verify on, and merge-preview files. | Upheld. Ruling R3. |
| B5 | Major | `success` is `failures.is_empty()`, but conflicts are not required to be failures. Preview success and CLI exit code are unspecified. | Upheld. Ruling R3. |
| B6 | Minor | `binary_changed` applies when only the base is binary. | Accepted as is. Any NUL still means "never merge". The name is kept, and its definition says "a version is binary". |
| B7 | Minor/Major | `marked_text` is specified as UTF-8, but a non-UTF-8 text file produces non-UTF-8 markers. | Upheld and verified. Ruling R4. |
| B8 | Minor | A non-regular base entry (symlink or submodule) and a `git merge-file` error exit are undefined. | Upheld. Ruling R5. |
| B9 | Minor | For a merged upload, `object_id` and `bytes` are ambiguous. | Upheld. Ruling R6. |
| B10 | Major | Compatibility 02 says "no server download", which contradicts default verification (it downloads after upload). | Upheld. Ruling R7 (wording fix). |
| B11 | Minor | No base-blob data source exists yet. | Not a defect. The spec declares it, and design D6 covers it. |
| B12 | Major | A planning failure next to clean merges still uploads the clean files, which is a partial deploy in all-or-nothing mode. | Upheld. Ruling R3. |
| — | Minor | The merge preview does not state that binary mode is selected before downloads. | Upheld. Ruling R7. |
| — | Doc | The proposal says NUL-anywhere is "the same test git uses". git only checks the first 8,000 bytes. | Fix the proposal wording. |

### Checker: server-drift-guard

| Lead | Severity | Summary | Adjudication |
| --- | --- | --- | --- |
| D1 | Major | A refused real run could still `mkdir`. `directories_created` and the dry-run counts on refusal are unspecified. | Upheld. Ruling R8. |
| D2 | Major | Non-branch FTP connections never select binary mode, so an ASCII-mode RETR can alter bytes and cause false drift. | Upheld. Ruling R8. |
| D3 | Major | Same 550 ambiguity as B1. | Upheld. Ruling R1. |
| D4 | Major | Precedence between drifted files and a download error or lost connection is unspecified. | Upheld. Ruling R9. |
| D5 | Major | "Target file" is undefined (ignored, deleted, and unreadable files). | Upheld. Ruling R9. |
| D6 | Minor | Whether `drifted[].remote_path` includes `remote_root` is unspecified. | Upheld. Ruling R8 (it includes the full path, as `UploadResponse.remote_path` does). |
| D7 | Minor | An expected path that is a tree or gitlink is conflated with "absent". | Upheld. Ruling R9. |
| D8 | Minor | The expected copy is raw blob bytes, but the upload copy is working-tree bytes (eol or filters). | Accepted as is. Compare raw bytes and document the effect of autocrlf and filters. |
| D9 | Minor | The zero-SHA example rejects only if the ref is peeled with `^{commit}`. | Upheld. Ruling R9 (always peel). |
| D10 | Minor | `ftp_upload_file` keys the expected copy by the local path, not by `remote_path`. | Accepted as is. This is intended, and it gets documented. |
| D11 | Major | `ftp_deploy` with a non-repo `local_root` and `expect_ref` has no outcome. | Upheld. Ruling R9. |
| D12 | Minor | Pre-existing bug: with a subdirectory `local_root`, `ftp_deploy_commits` silently uploads nothing, because `diff-tree` paths are repo-root-relative. | Out of scope. Filed as a separate suggested task. Recorded in design Risks. |

### Rulings

Brandon rules on R1–R9 in chat. Their outcomes are recorded below.

Ruled 2026-09-29: Brandon accepted all nine rulings (R1-R9) as recommended.
- R1: on a 550 reply, list the parent directory. Absent name → missing; present name → download failure.
- R2: new first rule, head blob == base blob → `unchanged_in_range`, no download, no upload.
- R3: add `merge_status: not_decided`; conflicts, download/planning/blob-read failures all block the run and each gets a `failures[]` record (so `success` false, CLI nonzero, preview included); blocked files `not_attempted`; `already_deployed` upload and verification `not_needed`; preview would-upload files `planned`.
- R4: marked_text is lossy UTF-8 (U+FFFD), truncated at 65,536 bytes on a char boundary.
- R5: a non-regular base entry counts as an absent base; a `git merge-file` error exit is a failure that blocks.
- R6: merged upload keeps head `object_id`; `bytes` reports uploaded bytes.
- R7: Compatibility 02 says "no download before upload"; merge preview selects binary mode before downloads.
- R8: refused drift run creates no directories, `files_uploaded` and `directories_created` 0; drift downloads use binary mode; `drifted[].remote_path` is the full server path.
- R9: any download error or lost connection during a drift check fails the tool with an error naming the file and nothing uploads; target file = a file the tool would upload; unreadable local file, non-blob expected path, or non-repo local_root with expect_ref → invalid-args error; expect_ref is peeled with `^{commit}`.
