## Purpose

Provide deterministic, worktree-aware FTP deployment of committed Git branch state with in-operation byte verification and an explicit, separately authorized path for remote deletions.
## Requirements
### Requirement: Branch deployment accepts an explicit repository and commit range
The system SHALL expose branch deployment through both the CLI and MCP. Each invocation SHALL accept a profile, an absolute repository root, a base ref, a head ref, and a deployment mode. The head ref SHALL default to `HEAD`, verification SHALL default to enabled, and the mode SHALL default to `overwrite`. The mode SHALL be exactly `overwrite` or `merge`.

#### Scenario: Deploy the default head
- **GIVEN** a valid profile and an absolute worktree root whose `HEAD` resolves to a commit
- **WHEN** a caller supplies the profile, the repository root, and a base ref without a head ref
- **THEN** the system plans the range from the resolved base commit through the commit resolved from `HEAD`

#### Scenario: Reject a directory that is not the selected worktree root
- **GIVEN** a supplied repository root that is <root_problem>
- **WHEN** a caller requests a branch deployment
- **THEN** the system rejects the invocation before accessing credentials or FTP

##### Examples
| root_problem |
| --- |
| relative |
| not a Git worktree |
| not the exact root of its worktree |

#### Scenario: Reject an unresolved ref
- **GIVEN** a valid worktree root in which the <ref_role> ref cannot be resolved to a commit
- **WHEN** a caller requests a branch deployment
- **THEN** the system rejects the invocation before accessing credentials or FTP

##### Examples
| ref_role |
| --- |
| base |
| head |

#### Scenario: Branch range 04 - mode defaults to overwrite
- **GIVEN** a valid profile, worktree root, and base ref
- **WHEN** a caller requests a branch deployment without a mode
- **THEN** the manifest reports mode `overwrite` and the deployment behaves as an overwrite deployment

#### Scenario: Branch range 05 - reject an unknown mode
- **GIVEN** a valid profile, worktree root, and base ref
- **WHEN** a caller requests a branch deployment with mode <bad_mode>
- **THEN** the system rejects the invocation before accessing credentials or FTP

##### Examples
| bad_mode |
| --- |
| rebase |
| MERGE |

### Requirement: Deployment includes every path touched in the full commit range
The system SHALL enumerate every commit in `base_ref..head_ref` and form a deterministic union of all Git paths touched by those commits. A merge commit SHALL contribute paths changed relative to its first parent, while commits reachable through the merged side SHALL remain part of the full range.

#### Scenario: Path changed in an intermediate commit
- **WHEN** a path changes in an intermediate commit and does not differ between the final head and base snapshots
- **THEN** the deployment plan includes that path

#### Scenario: Path changed more than once
- **WHEN** multiple commits in the range touch the same path
- **THEN** the deployment plan includes that path exactly once

#### Scenario: Merge commit resolves a path
- **WHEN** a merge commit changes a path relative to its first parent
- **THEN** the deployment plan includes that path and still accounts for paths touched by the merged-side commits in the range

### Requirement: Deployment uploads exact committed head blobs
For each touched path that is a regular blob at the resolved head commit, an overwrite-mode deployment SHALL upload the exact bytes of that head blob. A merge-mode deployment SHALL upload either the exact head blob bytes or the merged bytes that the merge-mode decision rules select for that path. Neither mode SHALL substitute working-tree bytes, profile `local_root` bytes, ignore-pattern filtering, or uncommitted changes.

#### Scenario: Working tree differs from the head commit
- **GIVEN** a planned path with uncommitted working-tree changes
- **WHEN** an overwrite-mode deployment uploads that path
- **THEN** the uploaded bytes equal the resolved head blob and the manifest reports that the repository is dirty

#### Scenario: Touched path survives at head
- **GIVEN** a touched path that exists as a regular blob at the resolved head commit
- **WHEN** the system plans the deployment
- **THEN** the plan contains its Git path, blob identifier, byte count, and mapped remote path

#### Scenario: Touched entry is not a deployable blob
- **GIVEN** a touched path that resolves to a non-blob entry, such as a submodule, at the head commit
- **WHEN** the system plans the deployment
- **THEN** the system rejects or records the path as a planning failure without uploading it

#### Scenario: Unsafe Git path
- **GIVEN** a touched path that would escape or ambiguously address the configured remote root
- **WHEN** the system plans the deployment
- **THEN** the system rejects or records the path as a planning failure without contacting FTP for that path

#### Scenario: Head blobs 05 - merge mode ignores working-tree changes
- **GIVEN** a planned path with uncommitted working-tree changes and a server copy equal to the base blob
- **WHEN** a merge-mode deployment uploads that path
- **THEN** the uploaded bytes equal the resolved head blob, not the working-tree bytes

### Requirement: Deployment reports removed Git paths without deleting remotely
The system SHALL report paths touched in the range that do not exist as deployable blobs at the resolved head commit. Branch deployment SHALL NOT delete remote files.

#### Scenario: Path deleted by the branch
- **WHEN** a path is deleted between the resolved base and head commits
- **THEN** the deployment manifest lists the path as requiring a separate explicit deletion call and performs no remote deletion

#### Scenario: Path removed and recreated
- **WHEN** a touched path exists as a regular blob at the resolved head commit
- **THEN** the system plans one upload for the final head blob rather than reporting the path as deleted

### Requirement: Actual deployment uses one binary FTP session
An actual branch deployment SHALL use one FTP connection, SHALL select binary transfer mode before any data operation, and SHALL process planned paths in deterministic Git-path order.

#### Scenario: Multiple files are deployed
- **WHEN** an actual deployment contains multiple planned uploads
- **THEN** all upload and verification operations use one FTP connection in deterministic path order

#### Scenario: Binary content is deployed
- **WHEN** a planned blob contains arbitrary binary bytes
- **THEN** upload and verification preserve those bytes without text conversion

### Requirement: Verification compares complete remote bytes
When verification is enabled, the system SHALL download each remote file immediately after its upload through the same FTP session and compare the complete remote byte stream with the bytes it uploaded for that path. In overwrite mode those bytes are the committed head blob. In merge mode they are the head blob or the merged bytes. The system SHALL consume the complete remote stream even after detecting a mismatch.

#### Scenario: Uploaded bytes match
- **GIVEN** verification is enabled and a file was uploaded in <mode> mode with <uploaded_from> bytes
- **WHEN** the complete remote byte stream equals the uploaded bytes
- **THEN** the file result records successful upload and verification with the number of remote bytes read

##### Examples
| mode | uploaded_from |
| --- | --- |
| overwrite | head_blob |
| merge | head_blob |
| merge | merged |

#### Scenario: Uploaded bytes differ
- **GIVEN** verification is enabled and a file was uploaded
- **WHEN** any remote byte differs from the uploaded bytes or the stream length differs
- **THEN** the file result records a verification mismatch and the overall manifest is unsuccessful

#### Scenario: Verification is disabled
- **GIVEN** a caller explicitly disables verification
- **WHEN** the deployment uploads planned files
- **THEN** the system uploads the planned files and records verification as not requested

### Requirement: Dry run performs no secret, content, or network access
An overwrite-mode dry run SHALL resolve and inspect Git metadata needed to produce the plan but SHALL NOT read Git blob contents, access saved credentials, open an FTP connection, upload, download, or delete remote data. A merge-mode dry run is a merge preview: it SHALL read blob contents, access saved credentials, open one FTP connection, select binary transfer mode before any download, and download server copies to apply the merge-mode decision rules, but it SHALL NOT upload, create remote directories, or delete remote data. In a merge preview, a file that would upload SHALL report `upload_status` `planned`, and every blocking failure SHALL be recorded as it would be in an actual run.

#### Scenario: Dry-run plan succeeds
- **GIVEN** a valid repository and range
- **WHEN** a caller requests an overwrite-mode dry run
- **THEN** the system returns the normal manifest shape with planned statuses and no credential, blob-content, or network access

#### Scenario: Dry run 02 - merge preview writes nothing remotely
- **GIVEN** a valid repository and range with <planned_files> planned files
- **WHEN** a caller requests a merge-mode dry run
- **THEN** the manifest reports a merge status for each of the <planned_files> files and the system performs no upload, directory creation, or deletion

##### Examples
| planned_files |
| --- |
| 1 |
| 3 |

#### Scenario: Dry run 03 - merge preview reports a conflict without uploading
- **GIVEN** a valid range in which one planned file conflicts with its server copy
- **WHEN** a caller requests a merge-mode dry run
- **THEN** the manifest reports that file as `conflict`, sets `blocked_by_conflicts` to true and `success` to false, and the system performs no upload

### Requirement: Deployment failures are classified and accumulated deterministically
The system SHALL distinguish a per-operation failure from a lost FTP connection. A per-operation failure SHALL be recorded and processing SHALL continue with the next path. A lost connection SHALL stop further remote attempts, record the triggering failure, and mark every remaining planned path as not attempted without reconnecting.

#### Scenario: One upload operation fails
- **WHEN** an upload or verification operation fails while the FTP connection remains usable
- **THEN** the manifest records the failure and the system attempts the next planned path

#### Scenario: FTP connection is lost
- **WHEN** an upload or verification operation reports a lost connection
- **THEN** the system makes no reconnection attempt and marks all remaining planned paths as not attempted

#### Scenario: Any file fails
- **WHEN** one or more planned files fail upload or verification
- **THEN** the returned manifest has `success` set to false

### Requirement: Deployment returns a complete structured manifest
The CLI and MCP SHALL return the same manifest semantics. The manifest SHALL include success, profile, repository root and dirty state, requested refs and resolved commit identifiers, merge rule, mode, `blocked_by_conflicts`, dry-run and verification settings, summary counts, ordered upload results, ordered removed-path reports, and ordered failures. Each upload result SHALL include Git path, remote path, blob identifier, byte count, upload status, and verification status. The blob identifier is always the head blob identifier. The byte count is the number of bytes uploaded, or planned for upload, for that path, which for a merged file is the merged byte count. In merge mode each upload result SHALL also include `merge_status`, and SHALL include `uploaded_from` when the file uploads or would upload. A conflict result SHALL also include `conflict_reason`, and a `text_conflict` result SHALL include `marked_text` and `marked_text_truncated`. Each failure SHALL include its stage, exact Git path when available, and error. `success` SHALL be true only when `failures` is empty.

Manifest field vocabulary:
- `mode` (required): `overwrite` | `merge`.
- `blocked_by_conflicts` (required): boolean. It is true only in merge mode, when the merge-mode blocking rule prevented uploads.
- `upload_status` (required): `planned` | `uploaded` | `failed` | `not_attempted` | `not_needed`. `not_needed` is used only in merge mode, for a file whose `merge_status` is `unchanged_in_range` or `already_deployed`.
- `verification_status` (required): `planned` | `verified` | `mismatch` | `not_requested` | `failed` | `not_attempted` | `not_needed`. `not_needed` is used only where `upload_status` is `not_needed`.
- `merge_status` (merge mode only, required there): `unchanged_in_range` | `already_deployed` | `fast_forward` | `merged` | `new_file` | `conflict` | `download_failed` | `not_decided`. `not_decided` marks a file the system never decided because the run stopped first.
- `uploaded_from` (merge mode only, optional): `head_blob` | `merged`.
- `conflict_reason` (required when `merge_status` is `conflict`): `text_conflict` | `binary_changed` | `deleted_on_server` | `added_on_both`.
- `marked_text` (required when `conflict_reason` is `text_conflict`): the merge output with conflict markers, converted to UTF-8 with each invalid byte sequence replaced by U+FFFD, and cut to at most 65,536 bytes without splitting a character.
- `marked_text_truncated` (required when `marked_text` is present): boolean.

#### Scenario: Deployment succeeds
- **GIVEN** a planned deployment in <mode> mode
- **WHEN** every planned upload and requested verification succeeds
- **THEN** the manifest reports mode <mode>, successful counts, `blocked_by_conflicts` false, and `success` set to true

##### Examples
| mode |
| --- |
| overwrite |
| merge |

#### Scenario: Execution completes unsuccessfully through MCP
- **GIVEN** planning has completed through MCP
- **WHEN** an upload or verification fails
- **THEN** MCP returns the full unsuccessful manifest rather than replacing it with an opaque tool error

#### Scenario: Execution completes unsuccessfully through the CLI
- **GIVEN** planning has completed through the CLI
- **WHEN** an upload or verification fails
- **THEN** the CLI emits the full unsuccessful manifest and exits with a nonzero status

#### Scenario: Manifest 04 - blocked merge is unsuccessful through the CLI
- **GIVEN** a merge-mode deployment or merge preview through the CLI in which one planned file conflicts
- **WHEN** the deployment finishes
- **THEN** the CLI emits the manifest with `blocked_by_conflicts` true and `success` false, and exits with a nonzero status

#### Scenario: Manifest 05 - long conflict text is truncated
- **GIVEN** a merge-mode text conflict whose marked text is <marked_bytes> bytes
- **WHEN** the manifest is produced
- **THEN** `marked_text` holds at most 65,536 bytes and `marked_text_truncated` is <truncated>

##### Examples
| marked_bytes | truncated |
| --- | --- |
| 200 | false |
| 65536 | false |
| 70000 | true |

#### Scenario: Manifest 06 - non-UTF-8 conflict text is readable
- **GIVEN** a merge-mode text conflict in a file whose lines contain the Latin-1 byte 0xE9
- **WHEN** the manifest is produced
- **THEN** `marked_text` is valid UTF-8 with U+FFFD in place of each 0xE9 byte

#### Scenario: Manifest 07 - merged upload reports head blob and uploaded size
- **GIVEN** a merge-mode file whose merged bytes differ in length from its head blob
- **WHEN** the file uploads
- **THEN** its result reports the head blob identifier, the merged byte count, and `uploaded_from` `merged`

### Requirement: Remote deletion requires a separate explicit invocation
The system SHALL expose remote branch-file deletion as a command and MCP tool separate from branch deployment. The invocation SHALL require a profile, absolute repository root, resolved base commit identifier, resolved head commit identifier, exact requested Git paths, and a non-empty reason. It SHALL accept only paths that are deleted between the supplied commits.

#### Scenario: Deployment reports deletion candidates
- **WHEN** branch deployment reports removed Git paths
- **THEN** no deletion occurs unless a caller later invokes the separate deletion operation with pinned commits, exact paths, and a reason

#### Scenario: Requested path is not in the pinned deleted set
- **WHEN** any requested deletion path is not deleted between the supplied commit identifiers
- **THEN** the deletion operation rejects the entire request before accessing credentials or FTP

#### Scenario: Commit identifier is not pinned and resolvable
- **WHEN** either supplied deletion endpoint is not an exact resolvable commit identifier
- **THEN** the deletion operation rejects the entire request before accessing credentials or FTP

### Requirement: Deletion preflight blocks ambiguous or unsafe requests atomically
Before accessing credentials or FTP, the deletion operation SHALL validate every requested path. It SHALL reject the entire call if any path is unsafe, protected, non-ASCII, duplicated under ASCII case-insensitive comparison, or case-collides with another relevant path. The response SHALL identify every blocked path and its reason.

#### Scenario: One requested path is unsafe
- **WHEN** one path in a multi-path deletion request fails preflight validation
- **THEN** no requested path is deleted and the response lists the blocked path and reason

#### Scenario: Requested paths collide by ASCII case
- **WHEN** two relevant paths compare equal under ASCII case-insensitive comparison
- **THEN** the deletion operation rejects the entire call before accessing credentials or FTP and reports the collision

#### Scenario: Requested path contains non-ASCII characters
- **WHEN** any requested deletion path contains non-ASCII characters
- **THEN** the deletion operation rejects the entire call before accessing credentials or FTP and reports the path as blocked

### Requirement: Approved deletion uses one FTP session and reports individual failures
After deletion preflight succeeds, the system SHALL use one FTP connection, select binary transfer mode before remote operations, attempt exact requested paths in deterministic order, continue after individual server deletion failures while the connection remains usable, and return a structured deletion manifest.

#### Scenario: One server deletion fails
- **WHEN** the server rejects deletion of one approved path without losing the connection
- **THEN** the system records that failure and attempts the remaining approved paths

#### Scenario: Connection is lost during deletion
- **WHEN** the FTP connection is lost while deleting an approved path
- **THEN** the system stops remote attempts, makes no reconnection attempt, and marks the remaining approved paths as not attempted

#### Scenario: Deletion dry run
- **WHEN** a caller requests a deletion dry run and preflight succeeds
- **THEN** the system returns planned deletion results without accessing credentials or FTP

### Requirement: Existing FTP operations remain compatible
Branch deployment, merge mode, and explicit branch-file deletion SHALL NOT change the inputs, outputs, or behavior of existing single-file, directory, and commit deployment operations, except for the optional `expect_ref` input and its drift-check result that the server-drift-guard capability defines. When a caller omits `expect_ref`, those operations SHALL behave exactly as before.

#### Scenario: Existing tool is invoked
- **GIVEN** an existing single-file, directory, or commit deployment tool
- **WHEN** a caller invokes it with the inputs it accepted before this change
- **THEN** it uploads the same files with the same bytes and returns the same response fields as before, with no server download

#### Scenario: Compatibility 02 - branch deployment without mode
- **GIVEN** a branch deployment request that names no mode
- **WHEN** a caller runs it, with or without a dry run
- **THEN** the system performs no server download before uploading and uploads exactly what it uploaded before this change

### Requirement: Merge mode decides each file from its base, head, and server copies
In merge mode, for each planned upload path, the system SHALL use the base blob at the resolved base commit, the head blob, and the server's current copy. The base blob is absent when the path does not exist at the base commit or is not a regular file there, such as a symbolic link or submodule. The server copy is missing only when the server answers the download with FTP status 550 and a listing of the parent directory does not contain the file name. When the server answers 550 and the listing contains the name, or the listing fails, the file is `download_failed`. A version is binary when it contains a NUL byte anywhere. The system SHALL apply these rules in order, and the first matching rule SHALL decide the file:

1. The base blob exists and equals the head blob: `unchanged_in_range`. The system does not download or upload the file.
2. The server copy exists and equals the head blob: `already_deployed`, and the file does not upload.
3. The base blob exists and the server copy equals it: `fast_forward`, and the head blob uploads.
4. The base blob exists, the server copy exists and differs, and no version is binary: the system three-way merges them. A clean merge is `merged`, and the merged bytes upload. An unclean merge is `conflict` with reason `text_conflict`. A merge tool error is a failure with stage `merge`.
5. The base blob exists, the server copy exists and differs, and any version is binary: `conflict` with reason `binary_changed`.
6. The base blob exists and the server copy is missing: `conflict` with reason `deleted_on_server`.
7. The base blob is absent and the server copy is missing: `new_file`, and the head blob uploads.
8. The base blob is absent and the server copy exists and differs from the head blob: `conflict` with reason `added_on_both`.

#### Scenario: Merge rules 01 - decision table
- **GIVEN** a merge-mode plan with one path whose base blob is <base>, whose server copy is <server>, and whose versions are <content_kind>
- **WHEN** the system decides the file
- **THEN** the file's merge status is <merge_status> and the file's upload source is <upload_source>

##### Examples
| base | server | content_kind | merge_status | upload_source |
| --- | --- | --- | --- | --- |
| equal to head | different from head | binary | unchanged_in_range | none |
| equal to head | missing | text | unchanged_in_range | none |
| present | equal to head | text | already_deployed | none |
| absent | equal to head | text | already_deployed | none |
| present | equal to base | text | fast_forward | head_blob |
| present | equal to base | binary | fast_forward | head_blob |
| present | changed on a different line than head | text | merged | merged |
| present | changed on the same line as head | text | conflict | none |
| present | changed | binary | conflict | none |
| present | missing | text | conflict | none |
| absent | missing | text | new_file | head_blob |
| absent | different from head | text | conflict | none |
| a symbolic link | missing | text | new_file | head_blob |

#### Scenario: Merge rules 02 - conflict reason is reported
- **GIVEN** a merge-mode plan with one path that conflicts because <situation>
- **WHEN** the manifest is produced
- **THEN** the file's `conflict_reason` is <conflict_reason> and `failures` contains a record with stage `merge` for that path

##### Examples
| situation | conflict_reason |
| --- | --- |
| base, head, and server edit the same text line differently | text_conflict |
| the server copy of a binary file differs from base and head | binary_changed |
| the server copy is missing but the base blob exists | deleted_on_server |
| the base blob is absent and the server copy differs from head | added_on_both |

#### Scenario: Merge rules 03 - server-only line is preserved
- **GIVEN** a text file whose server copy adds a line that exists in neither the base nor the head blob, and whose head blob changes a non-adjacent line
- **WHEN** a merge-mode deployment uploads that path
- **THEN** the uploaded bytes contain both the server-only line and the head change

#### Scenario: Merge rules 04 - adjacent edits conflict
- **GIVEN** a text file where the head blob changes line <head_line> and the server copy changes line <server_line>
- **WHEN** the system decides the file
- **THEN** the file's merge status is `conflict` with reason `text_conflict`

##### Examples
| head_line | server_line |
| --- | --- |
| 2 | 2 |
| 2 | 3 |

#### Scenario: Merge rules 05 - download failure is not treated as missing
- **GIVEN** a merge-mode plan where the server answers one download with <server_answer> and the connection stays usable
- **WHEN** the system decides the files
- **THEN** that file's merge status is `download_failed`, the manifest records a failure with stage `download`, and the system decides the remaining files

##### Examples
| server_answer |
| --- |
| a 451 local error |
| a 550 reply while the parent listing contains the file name |

#### Scenario: Merge rules 06 - a 550 for an absent file means missing
- **GIVEN** a merge-mode plan with a path added in the range, where the server answers 550 and the parent listing does not contain the file name
- **WHEN** the system decides the file
- **THEN** the file's merge status is `new_file`

### Requirement: Merge mode uploads nothing unless every file resolves
A merge-mode deployment SHALL decide every planned file before its first upload or directory creation. The run is blocked when any file is `conflict` or `download_failed`, the connection is lost or cannot be opened, binary mode cannot be selected, a blob cannot be read, the merge tool fails, or the plan contains a planning failure. A blocked run SHALL NOT upload any file, SHALL set `blocked_by_conflicts` to true, SHALL record each cause in `failures`, SHALL mark every file that is not `unchanged_in_range` or `already_deployed` as `upload_status` `not_attempted`, and SHALL mark every file it never decided as `not_decided`. An unblocked run SHALL upload the `fast_forward`, `merged`, and `new_file` files in deterministic Git-path order through the same FTP session.

#### Scenario: Merge blocking 01 - one conflict blocks clean files
- **GIVEN** a merge-mode plan with <clean_files> files that resolve cleanly and one file that conflicts
- **WHEN** the deployment runs
- **THEN** the system uploads no file, reports all <clean_files> clean files and the conflict file as `not_attempted`, and reports `blocked_by_conflicts` true and `success` false

##### Examples
| clean_files |
| --- |
| 1 |
| 4 |

#### Scenario: Merge blocking 02 - connection lost while downloading
- **GIVEN** a merge-mode plan with three files
- **WHEN** the connection is lost while downloading the second file
- **THEN** the system makes no reconnection attempt, uploads no file, reports the second and third files as `not_decided`, and reports `success` false

#### Scenario: Merge blocking 03 - everything resolves
- **GIVEN** a merge-mode plan whose files are `unchanged_in_range`, `fast_forward`, `merged`, `new_file`, and `already_deployed`
- **WHEN** the deployment runs
- **THEN** the system uploads exactly the `fast_forward`, `merged`, and `new_file` files, reports the `unchanged_in_range` and `already_deployed` files with upload and verification status `not_needed`, and reports `success` true

#### Scenario: Merge blocking 04 - planning failure blocks merge uploads
- **GIVEN** a merge-mode plan with two clean files and one path that is a planning failure
- **WHEN** the deployment runs
- **THEN** the system uploads no file and reports `blocked_by_conflicts` true and `success` false

