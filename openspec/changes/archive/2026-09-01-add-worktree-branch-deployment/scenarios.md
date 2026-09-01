# Branch deployment acceptance scenarios

These Gherkin scenarios refine the normative requirements in `specs/branch-deployment/spec.md`. The OpenSpec requirement remains authoritative if wording differs.

## Slice 1 scenarios

### Scenario S1.1: Resolve an explicit worktree and default head

```gherkin
Given an absolute path to the exact root of a Git worktree
And a base ref that resolves to a commit
When the caller requests a branch deployment dry run without a head ref
Then the planner resolves HEAD as the head commit
And the manifest records both requested refs and full commit IDs
```

### Scenario S1.2: Reject an invalid repository or ref before side effects

```gherkin
Given a relative path, a Git subdirectory, a non-worktree directory, or an unresolved ref
When the caller requests branch deployment
Then the operation returns invalid parameters
And it does not read credentials
And it does not open an FTP connection
```

### Scenario S1.3: Include the full range union

```gherkin
Given a path changed in an intermediate commit and restored before head
And another path changed more than once
When the planner enumerates base..head
Then both paths appear once in exact Git-path order
And each surviving path points to its head blob
```

### Scenario S1.4: Account for merge resolution and merged-side commits

```gherkin
Given base..head contains a merged side branch and a merge-resolution change
When the planner enumerates the range
Then it includes paths from commits reachable through the merged side
And it compares the merge commit with its first parent
```

### Scenario S1.5: Ignore mutable working-tree bytes and profile filters

```gherkin
Given a surviving head blob whose working-tree file is dirty
And profile local_root and ignore rules that point elsewhere
When the planner creates the upload entry
Then the entry contains the committed head blob ID and size
And the manifest reports the repository as dirty
And profile filters do not remove the entry
```

### Scenario S1.6: Report removals without remote deletion

```gherkin
Given a touched path is absent from the head tree
When branch deployment creates its manifest
Then the deleted entry is requires_explicit_call
And branch deployment performs no remote deletion
```

### Scenario S1.7: Reject unsafe or non-blob entries

```gherkin
Given a touched entry is a submodule, is not valid UTF-8, or maps outside the remote root
When the planner evaluates the entry
Then it records a planning failure for the exact path when representable
And it does not plan an upload for that entry
```

### Scenario S1.8: Dry run reads metadata only

```gherkin
Given a valid branch deployment request with dry_run true
When the operation completes
Then the manifest has planned upload statuses
And no Git blob content is read
And no credential or FTP operation occurs
```

## Slice 2 scenarios

### Scenario S2.1: Upload and verify binary bytes in one session

```gherkin
Given a valid plan with more than one binary blob
When actual branch deployment runs with verification enabled
Then it opens one FTP session
And it selects binary mode before data operations
And it uploads paths in exact Git-path order
And it compares each complete remote byte stream immediately after upload
```

### Scenario S2.2: Record matching and mismatching verification

```gherkin
Given one remote file matches its committed blob and another differs
When verification completes
Then the matching entry records verified and its remote byte count
And the differing entry records mismatch after draining the remote stream
And the manifest success value is false
```

### Scenario S2.3: Skip comparison only when requested

```gherkin
Given a valid actual deployment request with verification disabled
When upload succeeds
Then verification status is not_requested
And no RETR operation occurs
```

### Scenario S2.4: Continue after an operation failure

```gherkin
Given one upload or comparison returns an Operation failure
When more planned paths remain and the session is usable
Then the manifest records the exact failure
And execution attempts the next path
```

### Scenario S2.5: Stop after connection loss

```gherkin
Given one remote operation returns ConnectionLost
When more planned paths remain
Then execution does not reconnect
And every remaining path is not_attempted
And the manifest success value is false
```

### Scenario S2.6: Return complete unsuccessful results through both interfaces

```gherkin
Given planning succeeded and remote execution later fails
When MCP handles the request
Then MCP returns the complete unsuccessful manifest as data
When the CLI handles the same request
Then the CLI prints the complete unsuccessful manifest and exits nonzero
```

## Slice 3 scenarios

### Scenario S3.1: Require a separate explicit deletion call

```gherkin
Given branch deployment reported a deleted Git path
When no delete-branch-files call is made
Then the remote path remains untouched
```

### Scenario S3.2: Bind deletion to pinned commits and exact paths

```gherkin
Given full base and head commit IDs, exact requested paths, and a non-empty reason
When every requested path belongs to the recomputed deleted set
Then deletion preflight authorizes those exact paths
But when either commit is not a full canonical ID or any path is outside the deleted set
Then the entire request is rejected before credential or FTP access
```

### Scenario S3.3: Reject all paths when one path is unsafe

```gherkin
Given a deletion request with one safe path and one unsafe, protected, non-UTF-8, or non-ASCII path
When preflight runs
Then it reports every blocked path and reason
And it performs no remote deletion for any requested path
```

### Scenario S3.4: Block ASCII case collisions

```gherkin
Given a requested deletion ASCII-case-collides with another requested path or a surviving head blob
When preflight runs
Then it reports the collision
And it performs no credential or FTP operation
```

### Scenario S3.5: Delete approved paths deterministically

```gherkin
Given deletion preflight authorized every requested path
When deletion executes
Then it uses one binary FTP session
And it attempts exact paths in deterministic order
And an Operation failure does not block the next path
And ConnectionLost marks remaining paths not_attempted without reconnecting
```

### Scenario S3.6: Deletion dry run has no remote side effect

```gherkin
Given deletion preflight succeeds and dry_run is true
When the operation completes
Then every approved path has planned status
And no credential or FTP operation occurs
```

### Scenario S3.7: Preserve existing commands and tools

```gherkin
Given an existing single-file, directory, full-tree, or commit deployment invocation
When the new branch operations are present
Then the existing invocation keeps its previous inputs, outputs, and behavior
```
