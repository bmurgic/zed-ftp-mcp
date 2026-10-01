# server-drift-guard Specification

## Purpose
TBD - created by archiving change merge-aware-deploy. Update Purpose after archive.
## Requirements
### Requirement: Upload tools accept an optional expected ref
The single-file upload, directory deployment, and commit deployment MCP tools SHALL accept an optional `expect_ref` input naming a Git ref. When `expect_ref` is omitted, each tool SHALL perform no server download for drift checking and SHALL behave as it did before this capability existed, except that each tool SHALL reject a remote path that contains an empty, `.`, or `..` component, a backslash, or a control character. The repository is the Git worktree that contains the uploaded file (single-file upload) or the profile's local root (directory and commit deployment). The tool SHALL resolve `expect_ref` to a commit, peeling tags and requiring the object to exist. The tool SHALL reject the request with an invalid-arguments error before opening an FTP connection when no such repository exists, when `expect_ref` does not resolve to a commit, when a target file's local bytes cannot be read, or when a target file's path at `expect_ref` is a directory or submodule rather than a regular file.

#### Scenario: Expected ref 01 - omitted expected ref keeps overwrite behavior
- **GIVEN** a server file that differs from both the local file and every committed version
- **WHEN** a caller runs the <tool> tool for that file without `expect_ref`
- **THEN** the tool overwrites the server file and its response contains no drift-check result

##### Examples
| tool |
| --- |
| single-file upload |
| directory deployment |
| commit deployment |

#### Scenario: Expected ref 02 - unresolvable expected ref is rejected
- **GIVEN** a repository in which the ref <bad_ref> does not resolve to a commit
- **WHEN** a caller runs the <tool> tool with `expect_ref` set to <bad_ref>
- **THEN** the tool returns an invalid-arguments error and opens no FTP connection

##### Examples
| tool | bad_ref |
| --- | --- |
| single-file upload | no-such-branch |
| directory deployment | no-such-branch |
| commit deployment | 0000000000000000000000000000000000000000 |

#### Scenario: Expected ref 03 - upload source outside a repository
- **GIVEN** <source> that is not inside a Git worktree
- **WHEN** a caller runs the <tool> tool with `expect_ref` set
- **THEN** the tool returns an invalid-arguments error and opens no FTP connection

##### Examples
| source | tool |
| --- | --- |
| a local file | single-file upload |
| a profile local root | directory deployment |

#### Scenario: Expected ref 04 - expected path is not a regular file
- **GIVEN** a target file whose path at `expect_ref` is a directory
- **WHEN** a caller runs a deployment with `expect_ref`
- **THEN** the tool returns an invalid-arguments error naming the path and opens no FTP connection

#### Scenario: Expected ref 05 - unreadable local file
- **GIVEN** a directory deployment in which one target file cannot be read
- **WHEN** a caller runs it with `expect_ref`
- **THEN** the tool returns an invalid-arguments error naming the file and uploads nothing

### Requirement: Drift check classifies each target file
When `expect_ref` is supplied, the tool SHALL select binary transfer mode and download each target file's current server copy before uploading anything or creating any directory. A target file is a file the tool would upload without `expect_ref`; files excluded by ignore rules or absent from disk are not target files. A server copy is missing only when the server answers with FTP status 550 and a listing of the parent directory does not contain the file name. Any other download outcome, including a 550 whose parent listing contains the name, a failed listing, or a lost connection, SHALL make the tool return an error naming the file, and the tool SHALL upload nothing, even when other files have already been classified as drifted. The expected copy is the file's content at `expect_ref`, found by the file's path inside its repository, and it may be absent. The upload copy is the exact bytes the tool would upload. The tool SHALL classify each file as follows:

- clean: the server copy exists and equals the expected copy or the upload copy.
- clean: the server copy is missing and the expected copy is absent.
- drifted, reason `content_differs`: the server copy exists and equals neither copy.
- drifted, reason `missing_on_server`: the server copy is missing and the expected copy exists.

#### Scenario: Drift classification 01 - classification table
- **GIVEN** a target file whose server copy is <server> and whose content at `expect_ref` is <expected>
- **WHEN** the tool runs the drift check
- **THEN** the file is classified as <classification>

##### Examples
| server | expected | classification |
| --- | --- | --- |
| equal to the expected copy | present | clean |
| equal to the upload copy | present | clean |
| equal to the upload copy | absent | clean |
| missing | absent | clean |
| different from both copies | present | content_differs |
| different from the upload copy | absent | content_differs |
| missing | present | missing_on_server |

#### Scenario: Drift classification 02 - single-file upload of the committed version
- **GIVEN** a single-file upload with `before_changes` set, and a server copy equal to the file at HEAD
- **WHEN** the tool runs the drift check
- **THEN** the file is clean because the upload copy is the HEAD version

#### Scenario: Drift classification 03 - download error is not treated as missing
- **GIVEN** a deployment with one drifted file and one target file whose server download ends with <failure>
- **WHEN** the tool runs the drift check
- **THEN** the tool returns an error naming the failing file, uploads nothing, and creates no directory

##### Examples
| failure |
| --- |
| a 451 local error |
| a 550 reply while the parent listing contains the file name |
| a lost connection |

### Requirement: Any drifted file refuses the whole run
When at least one target file is drifted, the tool SHALL upload no file, SHALL create no directory, and SHALL return a response whose drift-check result lists every drifted file. A refused actual run SHALL report zero files uploaded, zero bytes uploaded, and zero directories created. When no file is drifted, the tool SHALL upload exactly the files and bytes it would have uploaded without `expect_ref`.

Drift-check result shape (present only when `expect_ref` is supplied):
- `expect_ref` (required): the ref as requested.
- `resolved_commit` (required): the full commit identifier it resolved to.
- `checked` (required): the number of target files checked.
- `refused` (required): boolean, true when at least one file drifted.
- `drifted` (required, may be empty): ordered list of entries, each with `remote_path` (required, the full server path including the profile's remote root) and `reason` (required, `content_differs` | `missing_on_server`).

#### Scenario: Drift refusal 01 - one drifted file blocks the others
- **GIVEN** a directory or commit deployment with <clean_files> clean target files and one drifted target file
- **WHEN** a caller runs it with `expect_ref`
- **THEN** the tool uploads no file, creates no directory, reports zero files uploaded and zero directories created, sets `refused` true, and lists exactly the drifted file with its full server path and reason

##### Examples
| clean_files |
| --- |
| 0 |
| 3 |

#### Scenario: Drift refusal 02 - no drift uploads normally
- **GIVEN** a deployment whose target files are all clean
- **WHEN** a caller runs it with `expect_ref`
- **THEN** the tool uploads the same files and bytes as a run without `expect_ref`, sets `refused` false, and lists no drifted files

#### Scenario: Drift refusal 03 - single-file upload refused
- **GIVEN** a single-file upload whose server copy differs from both the expected copy and the upload copy
- **WHEN** a caller runs it with `expect_ref`
- **THEN** the tool leaves the server file unchanged, reports zero bytes uploaded, and lists the file as `content_differs`

### Requirement: Dry run with an expected ref checks drift without uploading
When a directory or commit deployment runs with `dry_run` set and `expect_ref` supplied, the tool SHALL perform the drift check online and return the planned file list and planned counts exactly as a dry run without `expect_ref` reports them, together with the drift-check result, and SHALL NOT upload any file or create any directory. A dry run without `expect_ref` SHALL remain offline.

#### Scenario: Drift dry run 01 - drift is reported without uploading
- **GIVEN** a directory or commit deployment with one drifted target file
- **WHEN** a caller runs it with `dry_run` set and `expect_ref` supplied
- **THEN** the response lists the planned files and the drifted file, and the server is unchanged

#### Scenario: Drift dry run 02 - dry run without expected ref stays offline
- **GIVEN** a directory or commit deployment
- **WHEN** a caller runs it with `dry_run` set and no `expect_ref`
- **THEN** the tool opens no FTP connection and returns no drift-check result

