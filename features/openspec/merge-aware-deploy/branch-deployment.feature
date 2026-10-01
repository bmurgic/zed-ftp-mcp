# Generated from OpenSpec. Do not edit behavior in this file.
Feature: branch-deployment

  # Covers: branch-deployment / Branch deployment accepts an explicit repository and commit range / Deploy the default head
  Scenario: Deploy the default head
    Given a valid profile and an absolute worktree root whose `HEAD` resolves to a commit
    When a caller supplies the profile, the repository root, and a base ref without a head ref
    Then the system plans the range from the resolved base commit through the commit resolved from `HEAD`

  # Covers: branch-deployment / Branch deployment accepts an explicit repository and commit range / Reject a directory that is not the selected worktree root
  Scenario Outline: Reject a directory that is not the selected worktree root
    Given a supplied repository root that is <root_problem>
    When a caller requests a branch deployment
    Then the system rejects the invocation before accessing credentials or FTP

    Examples:
      | root_problem |
      | relative |
      | not a Git worktree |
      | not the exact root of its worktree |

  # Covers: branch-deployment / Branch deployment accepts an explicit repository and commit range / Reject an unresolved ref
  Scenario Outline: Reject an unresolved ref
    Given a valid worktree root in which the <ref_role> ref cannot be resolved to a commit
    When a caller requests a branch deployment
    Then the system rejects the invocation before accessing credentials or FTP

    Examples:
      | ref_role |
      | base |
      | head |

  # Covers: branch-deployment / Branch deployment accepts an explicit repository and commit range / Branch range 04 - mode defaults to overwrite
  Scenario: Branch range 04 - mode defaults to overwrite
    Given a valid profile, worktree root, and base ref
    When a caller requests a branch deployment without a mode
    Then the manifest reports mode `overwrite` and the deployment behaves as an overwrite deployment

  # Covers: branch-deployment / Branch deployment accepts an explicit repository and commit range / Branch range 05 - reject an unknown mode
  Scenario Outline: Branch range 05 - reject an unknown mode
    Given a valid profile, worktree root, and base ref
    When a caller requests a branch deployment with mode <bad_mode>
    Then the system rejects the invocation before accessing credentials or FTP

    Examples:
      | bad_mode |
      | rebase |
      | MERGE |

  # Covers: branch-deployment / Deployment uploads exact committed head blobs / Working tree differs from the head commit
  Scenario: Working tree differs from the head commit
    Given a planned path with uncommitted working-tree changes
    When an overwrite-mode deployment uploads that path
    Then the uploaded bytes equal the resolved head blob and the manifest reports that the repository is dirty

  # Covers: branch-deployment / Deployment uploads exact committed head blobs / Touched path survives at head
  Scenario: Touched path survives at head
    Given a touched path that exists as a regular blob at the resolved head commit
    When the system plans the deployment
    Then the plan contains its Git path, blob identifier, byte count, and mapped remote path

  # Covers: branch-deployment / Deployment uploads exact committed head blobs / Touched entry is not a deployable blob
  Scenario: Touched entry is not a deployable blob
    Given a touched path that resolves to a non-blob entry, such as a submodule, at the head commit
    When the system plans the deployment
    Then the system rejects or records the path as a planning failure without uploading it

  # Covers: branch-deployment / Deployment uploads exact committed head blobs / Unsafe Git path
  Scenario: Unsafe Git path
    Given a touched path that would escape or ambiguously address the configured remote root
    When the system plans the deployment
    Then the system rejects or records the path as a planning failure without contacting FTP for that path

  # Covers: branch-deployment / Deployment uploads exact committed head blobs / Head blobs 05 - merge mode ignores working-tree changes
  Scenario: Head blobs 05 - merge mode ignores working-tree changes
    Given a planned path with uncommitted working-tree changes and a server copy equal to the base blob
    When a merge-mode deployment uploads that path
    Then the uploaded bytes equal the resolved head blob, not the working-tree bytes

  # Covers: branch-deployment / Verification compares complete remote bytes / Uploaded bytes match
  Scenario Outline: Uploaded bytes match
    Given verification is enabled and a file was uploaded in <mode> mode with <uploaded_from> bytes
    When the complete remote byte stream equals the uploaded bytes
    Then the file result records successful upload and verification with the number of remote bytes read

    Examples:
      | mode | uploaded_from |
      | overwrite | head_blob |
      | merge | head_blob |
      | merge | merged |

  # Covers: branch-deployment / Verification compares complete remote bytes / Uploaded bytes differ
  Scenario: Uploaded bytes differ
    Given verification is enabled and a file was uploaded
    When any remote byte differs from the uploaded bytes or the stream length differs
    Then the file result records a verification mismatch and the overall manifest is unsuccessful

  # Covers: branch-deployment / Verification compares complete remote bytes / Verification is disabled
  Scenario: Verification is disabled
    Given a caller explicitly disables verification
    When the deployment uploads planned files
    Then the system uploads the planned files and records verification as not requested

  # Covers: branch-deployment / Dry run performs no secret, content, or network access / Dry-run plan succeeds
  Scenario: Dry-run plan succeeds
    Given a valid repository and range
    When a caller requests an overwrite-mode dry run
    Then the system returns the normal manifest shape with planned statuses and no credential, blob-content, or network access

  # Covers: branch-deployment / Dry run performs no secret, content, or network access / Dry run 02 - merge preview writes nothing remotely
  Scenario Outline: Dry run 02 - merge preview writes nothing remotely
    Given a valid repository and range with <planned_files> planned files
    When a caller requests a merge-mode dry run
    Then the manifest reports a merge status for each of the <planned_files> files and the system performs no upload, directory creation, or deletion

    Examples:
      | planned_files |
      | 1 |
      | 3 |

  # Covers: branch-deployment / Dry run performs no secret, content, or network access / Dry run 03 - merge preview reports a conflict without uploading
  Scenario: Dry run 03 - merge preview reports a conflict without uploading
    Given a valid range in which one planned file conflicts with its server copy
    When a caller requests a merge-mode dry run
    Then the manifest reports that file as `conflict`, sets `blocked_by_conflicts` to true and `success` to false, and the system performs no upload

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Deployment succeeds
  Scenario Outline: Deployment succeeds
    Given a planned deployment in <mode> mode
    When every planned upload and requested verification succeeds
    Then the manifest reports mode <mode>, successful counts, `blocked_by_conflicts` false, and `success` set to true

    Examples:
      | mode |
      | overwrite |
      | merge |

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Execution completes unsuccessfully through MCP
  Scenario: Execution completes unsuccessfully through MCP
    Given planning has completed through MCP
    When an upload or verification fails
    Then MCP returns the full unsuccessful manifest rather than replacing it with an opaque tool error

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Execution completes unsuccessfully through the CLI
  Scenario: Execution completes unsuccessfully through the CLI
    Given planning has completed through the CLI
    When an upload or verification fails
    Then the CLI emits the full unsuccessful manifest and exits with a nonzero status

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Manifest 04 - blocked merge is unsuccessful through the CLI
  Scenario: Manifest 04 - blocked merge is unsuccessful through the CLI
    Given a merge-mode deployment or merge preview through the CLI in which one planned file conflicts
    When the deployment finishes
    Then the CLI emits the manifest with `blocked_by_conflicts` true and `success` false, and exits with a nonzero status

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Manifest 05 - long conflict text is truncated
  Scenario Outline: Manifest 05 - long conflict text is truncated
    Given a merge-mode text conflict whose marked text is <marked_bytes> bytes
    When the manifest is produced
    Then `marked_text` holds at most 65,536 bytes and `marked_text_truncated` is <truncated>

    Examples:
      | marked_bytes | truncated |
      | 200 | false |
      | 65536 | false |
      | 70000 | true |

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Manifest 06 - non-UTF-8 conflict text is readable
  Scenario: Manifest 06 - non-UTF-8 conflict text is readable
    Given a merge-mode text conflict in a file whose lines contain the Latin-1 byte 0xE9
    When the manifest is produced
    Then `marked_text` is valid UTF-8 with U+FFFD in place of each 0xE9 byte

  # Covers: branch-deployment / Deployment returns a complete structured manifest / Manifest 07 - merged upload reports head blob and uploaded size
  Scenario: Manifest 07 - merged upload reports head blob and uploaded size
    Given a merge-mode file whose merged bytes differ in length from its head blob
    When the file uploads
    Then its result reports the head blob identifier, the merged byte count, and `uploaded_from` `merged`

  # Covers: branch-deployment / Existing FTP operations remain compatible / Existing tool is invoked
  Scenario: Existing tool is invoked
    Given an existing single-file, directory, or commit deployment tool
    When a caller invokes it with the inputs it accepted before this change
    Then it uploads the same files with the same bytes and returns the same response fields as before, with no server download

  # Covers: branch-deployment / Existing FTP operations remain compatible / Compatibility 02 - branch deployment without mode
  Scenario: Compatibility 02 - branch deployment without mode
    Given a branch deployment request that names no mode
    When a caller runs it, with or without a dry run
    Then the system performs no server download before uploading and uploads exactly what it uploaded before this change

  # Covers: branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 01 - decision table
  Scenario Outline: Merge rules 01 - decision table
    Given a merge-mode plan with one path whose base blob is <base>, whose server copy is <server>, and whose versions are <content_kind>
    When the system decides the file
    Then the file's merge status is <merge_status> and the file's upload source is <upload_source>

    Examples:
      | base | server | content_kind | merge_status | upload_source |
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

  # Covers: branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 02 - conflict reason is reported
  Scenario Outline: Merge rules 02 - conflict reason is reported
    Given a merge-mode plan with one path that conflicts because <situation>
    When the manifest is produced
    Then the file's `conflict_reason` is <conflict_reason> and `failures` contains a record with stage `merge` for that path

    Examples:
      | situation | conflict_reason |
      | base, head, and server edit the same text line differently | text_conflict |
      | the server copy of a binary file differs from base and head | binary_changed |
      | the server copy is missing but the base blob exists | deleted_on_server |
      | the base blob is absent and the server copy differs from head | added_on_both |

  # Covers: branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 03 - server-only line is preserved
  Scenario: Merge rules 03 - server-only line is preserved
    Given a text file whose server copy adds a line that exists in neither the base nor the head blob, and whose head blob changes a non-adjacent line
    When a merge-mode deployment uploads that path
    Then the uploaded bytes contain both the server-only line and the head change

  # Covers: branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 04 - adjacent edits conflict
  Scenario Outline: Merge rules 04 - adjacent edits conflict
    Given a text file where the head blob changes line <head_line> and the server copy changes line <server_line>
    When the system decides the file
    Then the file's merge status is `conflict` with reason `text_conflict`

    Examples:
      | head_line | server_line |
      | 2 | 2 |
      | 2 | 3 |

  # Covers: branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 05 - download failure is not treated as missing
  Scenario Outline: Merge rules 05 - download failure is not treated as missing
    Given a merge-mode plan where the server answers one download with <server_answer> and the connection stays usable
    When the system decides the files
    Then that file's merge status is `download_failed`, the manifest records a failure with stage `download`, and the system decides the remaining files

    Examples:
      | server_answer |
      | a 451 local error |
      | a 550 reply while the parent listing contains the file name |

  # Covers: branch-deployment / Merge mode decides each file from its base, head, and server copies / Merge rules 06 - a 550 for an absent file means missing
  Scenario: Merge rules 06 - a 550 for an absent file means missing
    Given a merge-mode plan with a path added in the range, where the server answers 550 and the parent listing does not contain the file name
    When the system decides the file
    Then the file's merge status is `new_file`

  # Covers: branch-deployment / Merge mode uploads nothing unless every file resolves / Merge blocking 01 - one conflict blocks clean files
  Scenario Outline: Merge blocking 01 - one conflict blocks clean files
    Given a merge-mode plan with <clean_files> files that resolve cleanly and one file that conflicts
    When the deployment runs
    Then the system uploads no file, reports all <clean_files> clean files and the conflict file as `not_attempted`, and reports `blocked_by_conflicts` true and `success` false

    Examples:
      | clean_files |
      | 1 |
      | 4 |

  # Covers: branch-deployment / Merge mode uploads nothing unless every file resolves / Merge blocking 02 - connection lost while downloading
  Scenario: Merge blocking 02 - connection lost while downloading
    Given a merge-mode plan with three files
    When the connection is lost while downloading the second file
    Then the system makes no reconnection attempt, uploads no file, reports the second and third files as `not_decided`, and reports `success` false

  # Covers: branch-deployment / Merge mode uploads nothing unless every file resolves / Merge blocking 03 - everything resolves
  Scenario: Merge blocking 03 - everything resolves
    Given a merge-mode plan whose files are `unchanged_in_range`, `fast_forward`, `merged`, `new_file`, and `already_deployed`
    When the deployment runs
    Then the system uploads exactly the `fast_forward`, `merged`, and `new_file` files, reports the `unchanged_in_range` and `already_deployed` files with upload and verification status `not_needed`, and reports `success` true

  # Covers: branch-deployment / Merge mode uploads nothing unless every file resolves / Merge blocking 04 - planning failure blocks merge uploads
  Scenario: Merge blocking 04 - planning failure blocks merge uploads
    Given a merge-mode plan with two clean files and one path that is a planning failure
    When the deployment runs
    Then the system uploads no file and reports `blocked_by_conflicts` true and `success` false
