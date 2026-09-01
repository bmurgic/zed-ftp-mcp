## Purpose

Provide deterministic, worktree-aware FTP deployment of committed Git branch state with in-operation byte verification and an explicit, separately authorized path for remote deletions.

## Requirements

### Requirement: Branch deployment accepts an explicit repository and commit range
The system SHALL expose branch deployment through both the CLI and MCP. Each invocation SHALL accept a profile, an absolute repository root, a base ref, and a head ref. The head ref SHALL default to `HEAD`, and verification SHALL default to enabled.

#### Scenario: Deploy the default head
- **WHEN** a caller supplies a profile, an absolute repository root, and a base ref without a head ref
- **THEN** the system plans the range from the resolved base commit through the commit resolved from `HEAD`

#### Scenario: Reject a directory that is not the selected worktree root
- **WHEN** the supplied repository root is relative, is not a Git worktree, or is not the exact root of that worktree
- **THEN** the system rejects the invocation before accessing credentials or FTP

#### Scenario: Reject an unresolved ref
- **WHEN** the base ref or head ref cannot be resolved to a commit in the selected repository
- **THEN** the system rejects the invocation before accessing credentials or FTP

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
For each touched path that is a regular blob at the resolved head commit, the system SHALL upload the exact bytes of that head blob. It SHALL NOT substitute working-tree bytes, profile `local_root` bytes, ignore-pattern filtering, or uncommitted changes.

#### Scenario: Working tree differs from the head commit
- **WHEN** a planned path has uncommitted working-tree changes
- **THEN** the uploaded bytes equal the resolved head blob and the manifest reports that the repository is dirty

#### Scenario: Touched path survives at head
- **WHEN** a touched path exists as a regular blob at the resolved head commit
- **THEN** the plan contains its Git path, blob identifier, byte count, and mapped remote path

#### Scenario: Touched entry is not a deployable blob
- **WHEN** a touched path resolves to a non-blob entry such as a submodule at the head commit
- **THEN** the system rejects or records the path as a planning failure without uploading it

#### Scenario: Unsafe Git path
- **WHEN** a touched path would escape or ambiguously address the configured remote root
- **THEN** the system rejects or records the path as a planning failure without contacting FTP for that path

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
When verification is enabled, the system SHALL download each remote file immediately after its upload through the same FTP session and compare the complete remote byte stream with the committed blob. The system SHALL consume the complete remote stream even after detecting a mismatch.

#### Scenario: Uploaded bytes match
- **WHEN** the complete remote byte stream equals the committed blob
- **THEN** the file result records successful upload and verification with the number of remote bytes read

#### Scenario: Uploaded bytes differ
- **WHEN** any remote byte differs from the committed blob or the stream length differs
- **THEN** the file result records a verification mismatch and the overall manifest is unsuccessful

#### Scenario: Verification is disabled
- **WHEN** a caller explicitly disables verification
- **THEN** the system uploads planned blobs and records verification as not requested

### Requirement: Dry run performs no secret, content, or network access
Dry-run branch deployment SHALL resolve and inspect Git metadata needed to produce the plan but SHALL NOT read Git blob contents, access saved credentials, open an FTP connection, upload, download, or delete remote data.

#### Scenario: Dry-run plan succeeds
- **WHEN** a caller requests a dry run for a valid repository and range
- **THEN** the system returns the normal manifest shape with planned statuses and no credential, blob-content, or network access

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
The CLI and MCP SHALL return the same manifest semantics. The manifest SHALL include success, profile, repository root and dirty state, requested refs and resolved commit identifiers, merge rule, dry-run and verification settings, summary counts, ordered upload results, ordered removed-path reports, and ordered failures. Each upload result SHALL include Git path, remote path, blob identifier, byte count, upload status, and verification status. Each failure SHALL include its stage, exact Git path when available, and error.

#### Scenario: Deployment succeeds
- **WHEN** every planned upload and requested verification succeeds
- **THEN** the manifest reports successful counts and `success` set to true

#### Scenario: Execution completes unsuccessfully through MCP
- **WHEN** planning has completed and an upload or verification fails
- **THEN** MCP returns the full unsuccessful manifest rather than replacing it with an opaque tool error

#### Scenario: Execution completes unsuccessfully through the CLI
- **WHEN** planning has completed and an upload or verification fails
- **THEN** the CLI emits the full unsuccessful manifest and exits with a nonzero status

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
The addition of branch deployment and explicit branch-file deletion SHALL NOT change the inputs, outputs, or behavior of existing single-file, directory, and commit deployment operations.

#### Scenario: Existing tool is invoked
- **WHEN** a caller invokes an existing FTP or commit-deployment command or MCP tool
- **THEN** it behaves as it did before this capability was added
