# End-to-end QA procedures

> The Gherkin scenarios in specs/ define required behavior.
> These procedures define how a tester proves that behavior through the application's public user interface.
>
> If a procedure conflicts with Gherkin, stop and resolve the specification.
> Do not silently change either artifact.
>
> Steps are user actions and observations only: no functions, test files, greps, or imports.
> If no automation can drive this interface, say so; do not convert procedures into unit tests.

Shared starting state for every procedure:
- A disposable FTP server that the tester owns, started from the pinned `delfer/alpine-ftp-server` image the project already uses, with user `test` and home `/home/test`. Never use the `staging` profile or any live server.
- A `connections.toml` profile named `qa` that points at that server with `remote_root = "/home/test"`, and its password saved with `zed-ftp-mcp set-password qa`.
- A throwaway Git repository `site` whose worktree root is the tester's absolute path `<site>`.
- The two public interfaces are the `zed-ftp-mcp` CLI, and the `zed-ftp-mcp serve` MCP server driven by an MCP client that sends `tools/call` requests over stdio. Slice QA may script the MCP client, because stdio JSON-RPC is the tool's public interface.
- Wherever a step inspects server content, the tester reads the file back through the MCP `ftp_download_file` tool.

## Feature: branch-deployment

### QA procedure: Merge a branch into a server that has its own edits

**Covers**
- `branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 01 - decision table`
- `branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 03 - server-only line is preserved`
- `branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 06 - a 550 for an absent file means missing`
- `branch-deployment / Merge mode uploads nothing unless every file resolves / Merge blocking 03 - everything resolves`
- `branch-deployment / Deployment returns a complete structured manifest / Deployment succeeds`
- `branch-deployment / Deployment returns a complete structured manifest / Manifest 07 - merged upload reports head blob and uploaded size`
- `branch-deployment / Verification compares complete remote bytes / Uploaded bytes match`
- `branch-deployment / Deployment uploads exact committed head blobs / Head blobs 05 - merge mode ignores working-tree changes`

**User interface**
- CLI

**Starting state**
- In `site`, commit `base` contains `Mailer.php` (lines `a` to `h`), `Mails.php` (lines `1` to `6`), `logo.bin` (bytes with a NUL), and `same.txt`.
- The server holds a deploy of `base`, except that `Mailer.php` has an extra line `BCC staging` after line `g`.
- Commit `head` on top of `base` changes line `b` of `Mailer.php`, changes line `2` of `Mails.php`, replaces `logo.bin`, adds `new.txt`, and changes `same.txt`. The server's `same.txt` is already set to the head content.
- `Mails.php` has an uncommitted working-tree edit.

**Procedure**

1. Run `zed-ftp-mcp deploy-branch qa --repo-root <site> --base base --head head --mode merge`.
   - **Observe:** The command exits 0. The printed manifest has `mode` `merge`, `blocked_by_conflicts` false, and `success` true.
2. Read the upload results in the manifest.
   - **Observe:** `Mailer.php` is `merged` with `uploaded_from` `merged`. Its `bytes` equals the merged size and differs from the head blob size, and `object_id` is the head blob ID. `Mails.php`, `logo.bin` are `fast_forward`. `new.txt` is `new_file`. `same.txt` is `already_deployed` with upload and verification status `not_needed`. Every uploaded file shows verification `verified`.
3. Download `Mailer.php` from the server.
   - **Observe:** It contains both the head change on line `b` and the `BCC staging` line.
4. Download `Mails.php` from the server.
   - **Observe:** It equals the committed head version, not the working-tree edit.

### QA procedure: A conflicting file blocks the whole merge

**Covers**
- `branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 02 - conflict reason is reported`
- `branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 04 - adjacent edits conflict`
- `branch-deployment / Merge mode uploads nothing unless every file resolves / Merge blocking 01 - one conflict blocks clean files`
- `branch-deployment / Deployment returns a complete structured manifest / Manifest 04 - blocked merge is unsuccessful through the CLI`
- `branch-deployment / Deployment returns a complete structured manifest / Manifest 06 - non-UTF-8 conflict text is readable`

**User interface**
- CLI

**Starting state**
- In `site`, commit `base` contains `Mails.php` (lines `1` to `6`, with line `4` containing the Latin-1 byte 0xE9), plus `a.txt` and `b.txt`. The server holds that deploy.
- The server's `Mails.php` has line `3` edited to `3 server`.
- Commit `head` changes line `2` of `Mails.php`, and it also changes `a.txt` and `b.txt`.

**Procedure**

1. Run `zed-ftp-mcp deploy-branch qa --repo-root <site> --base base --head head --mode merge`.
   - **Observe:** The command exits nonzero. The manifest shows `blocked_by_conflicts` true and `success` false.
2. Read the `Mails.php` result.
   - **Observe:** `merge_status` is `conflict`, `conflict_reason` is `text_conflict`, `upload_status` is `not_attempted`, and `marked_text` shows `<<<<<<<`, `|||||||`, `=======`, and `>>>>>>>` markers around lines `2` and `3`. The 0xE9 byte appears as the replacement character `�`. `failures` lists a `merge` stage record for `Mails.php`.
3. Read the `a.txt` and `b.txt` results.
   - **Observe:** Both are `fast_forward` with `upload_status` `not_attempted`.
4. Download `Mails.php`, `a.txt`, and `b.txt` from the server.
   - **Observe:** All three are unchanged from their state before step 1.

### QA procedure: Preview a merge without changing the server

**Covers**
- `branch-deployment / Dry run performs no secret, content, or network access / Dry run 02 - merge preview writes nothing remotely`
- `branch-deployment / Dry run performs no secret, content, or network access / Dry run 03 - merge preview reports a conflict without uploading`

**User interface**
- CLI

**Starting state**
- The same `site` repository and server state as "A conflicting file blocks the whole merge".

**Procedure**

1. Run the deploy command from that procedure with `--dry-run` added.
   - **Observe:** The command exits nonzero. The manifest has `dry_run` true, `mode` `merge`, `blocked_by_conflicts` true, and `success` false. `Mails.php` is `conflict`, and `a.txt` and `b.txt` are `fast_forward` with `upload_status` `planned`.
2. Download all three files from the server.
   - **Observe:** They are unchanged.
3. Restore the server's `Mails.php` to the `base` version, then repeat step 1.
   - **Observe:** The command exits 0. Every file shows a merge status, `blocked_by_conflicts` is false, and the server files are still unchanged.

### QA procedure: Overwrite deploys behave as before

**Covers**
- `branch-deployment / Branch deployment accepts an explicit repository and commit range / Branch range 04 - mode defaults to overwrite`
- `branch-deployment / Branch deployment accepts an explicit repository and commit range / Branch range 05 - reject an unknown mode`
- `branch-deployment / Existing FTP operations remain compatible / Compatibility 02 - branch deployment without mode`
- `branch-deployment / Dry run performs no secret, content, or network access / Dry-run plan succeeds`

**User interface**
- CLI

**Starting state**
- The same `site` repository and server state as "A conflicting file blocks the whole merge".
- For step 3, the FTP server container is stopped.

**Procedure**

1. Run `zed-ftp-mcp deploy-branch qa --repo-root <site> --base base --head head --mode rebase`.
   - **Observe:** The command fails with a usage error naming the allowed modes, and the server is unchanged.
2. Run `zed-ftp-mcp deploy-branch qa --repo-root <site> --base base --head head` with no mode.
   - **Observe:** The command exits 0, the manifest has `mode` `overwrite`, and it has no `merge_status` fields.
3. Download `Mails.php`.
   - **Observe:** It equals the head version, so the server-side edit to line `3` was overwritten, as it is today.
4. Stop the FTP server, then run step 2 again with `--dry-run`.
   - **Observe:** The command exits 0 and prints a planned manifest without any connection error.

## Feature: server-drift-guard

### QA procedure: Deploy a directory without clobbering server changes

**Covers**
- `server-drift-guard / Drift check classifies each target file / Drift classification 01 - classification table`
- `server-drift-guard / Any drifted file refuses the whole run / Drift refusal 01 - one drifted file blocks the others`
- `server-drift-guard / Any drifted file refuses the whole run / Drift refusal 02 - no drift uploads normally`
- `server-drift-guard / Dry run with an expected ref checks drift without uploading / Drift dry run 01 - drift is reported without uploading`
- `server-drift-guard / Dry run with an expected ref checks drift without uploading / Drift dry run 02 - dry run without expected ref stays offline`
- `server-drift-guard / Upload tools accept an optional expected ref / Expected ref 01 - omitted expected ref keeps overwrite behavior`

**User interface**
- MCP client

**Starting state**
- The `qa` profile's `local_root` is `<site>`. The server holds a deploy of `site` at commit `base`, which includes `a.txt`, `b.txt`, `c.txt`, and `sub/d.txt`.
- The working tree edits `a.txt`, `b.txt`, and `c.txt`.
- The server's `c.txt` has a server-only edit.

**Procedure**

1. Call `ftp_deploy` with `profile` `qa`, `dry_run` true, and `expect_ref` `base`.
   - **Observe:** The response lists the planned files, and its `drift_check` shows `refused` true with one entry: `/home/test/c.txt` with reason `content_differs`. The server is unchanged.
2. Call `ftp_deploy` with `profile` `qa` and `expect_ref` `base`.
   - **Observe:** `files_uploaded` and `directories_created` are 0, and `drift_check.refused` is true. Downloading `a.txt` shows it is still the `base` version.
3. Restore the server's `c.txt` to the `base` version, then repeat step 2.
   - **Observe:** The upload succeeds, `drift_check.refused` is false, `drifted` is empty, and the server files equal the working-tree files.
4. Put a server-only edit back into `c.txt`, then call `ftp_deploy` with `profile` `qa` and no `expect_ref`.
   - **Observe:** The response has no `drift_check`, and `c.txt` on the server is overwritten with the working-tree version.
5. Stop the FTP server, then call `ftp_deploy` with `dry_run` true and no `expect_ref`.
   - **Observe:** The response lists the planned files without any connection error.

### QA procedure: Upload one file only when the server matches what you expect

**Covers**
- `server-drift-guard / Drift check classifies each target file / Drift classification 02 - single-file upload of the committed version`
- `server-drift-guard / Any drifted file refuses the whole run / Drift refusal 03 - single-file upload refused`
- `server-drift-guard / Upload tools accept an optional expected ref / Expected ref 02 - unresolvable expected ref is rejected`
- `server-drift-guard / Upload tools accept an optional expected ref / Expected ref 03 - upload source outside a repository`

**User interface**
- MCP client

**Starting state**
- The server holds `Mails.php` equal to its version at `HEAD` in `site`. The working tree has an uncommitted edit to `Mails.php`.
- A file `/tmp/loose.txt` exists outside any Git repository.

**Procedure**

1. Call `ftp_upload_file` with `profile` `qa`, `local_path` `<site>/Mails.php`, `remote_path` `Mails.php`, `before_changes` true, and `expect_ref` `HEAD`.
   - **Observe:** The upload succeeds, and the `drift_check` shows `refused` false.
2. Edit `Mails.php` on the server to a new value, then call `ftp_upload_file` with `profile` `qa`, `local_path` `<site>/Mails.php`, `remote_path` `Mails.php`, and `expect_ref` `HEAD`.
   - **Observe:** `bytes` is 0, and `drift_check` lists `/home/test/Mails.php` as `content_differs`. The server copy keeps the edit from this step.
3. Call it again with `expect_ref` `no-such-branch`.
   - **Observe:** The tool returns an invalid-arguments error, and the server is unchanged.
4. Call `ftp_upload_file` with `local_path` `/tmp/loose.txt`, `remote_path` `loose.txt`, and `expect_ref` `HEAD`.
   - **Observe:** The tool returns an invalid-arguments error, and no `loose.txt` appears on the server.
