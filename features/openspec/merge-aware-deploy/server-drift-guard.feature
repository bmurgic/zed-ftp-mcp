# Generated from OpenSpec. Do not edit behavior in this file.
Feature: server-drift-guard

  # Covers: server-drift-guard / Upload tools accept an optional expected ref / Expected ref 01 - omitted expected ref keeps overwrite behavior
  Scenario Outline: Expected ref 01 - omitted expected ref keeps overwrite behavior
    Given a server file that differs from both the local file and every committed version
    When a caller runs the <tool> tool for that file without `expect_ref`
    Then the tool overwrites the server file and its response contains no drift-check result

    Examples:
      | tool |
      | single-file upload |
      | directory deployment |
      | commit deployment |

  # Covers: server-drift-guard / Upload tools accept an optional expected ref / Expected ref 02 - unresolvable expected ref is rejected
  Scenario Outline: Expected ref 02 - unresolvable expected ref is rejected
    Given a repository in which the ref <bad_ref> does not resolve to a commit
    When a caller runs the <tool> tool with `expect_ref` set to <bad_ref>
    Then the tool returns an invalid-arguments error and opens no FTP connection

    Examples:
      | tool | bad_ref |
      | single-file upload | no-such-branch |
      | directory deployment | no-such-branch |
      | commit deployment | 0000000000000000000000000000000000000000 |

  # Covers: server-drift-guard / Upload tools accept an optional expected ref / Expected ref 03 - upload source outside a repository
  Scenario Outline: Expected ref 03 - upload source outside a repository
    Given <source> that is not inside a Git worktree
    When a caller runs the <tool> tool with `expect_ref` set
    Then the tool returns an invalid-arguments error and opens no FTP connection

    Examples:
      | source | tool |
      | a local file | single-file upload |
      | a profile local root | directory deployment |

  # Covers: server-drift-guard / Upload tools accept an optional expected ref / Expected ref 04 - expected path is not a regular file
  Scenario: Expected ref 04 - expected path is not a regular file
    Given a target file whose path at `expect_ref` is a directory
    When a caller runs a deployment with `expect_ref`
    Then the tool returns an invalid-arguments error naming the path and opens no FTP connection

  # Covers: server-drift-guard / Upload tools accept an optional expected ref / Expected ref 05 - unreadable local file
  Scenario: Expected ref 05 - unreadable local file
    Given a directory deployment in which one target file cannot be read
    When a caller runs it with `expect_ref`
    Then the tool returns an invalid-arguments error naming the file and uploads nothing

  # Covers: server-drift-guard / Drift check classifies each target file / Drift classification 01 - classification table
  Scenario Outline: Drift classification 01 - classification table
    Given a target file whose server copy is <server> and whose content at `expect_ref` is <expected>
    When the tool runs the drift check
    Then the file is classified as <classification>

    Examples:
      | server | expected | classification |
      | equal to the expected copy | present | clean |
      | equal to the upload copy | present | clean |
      | equal to the upload copy | absent | clean |
      | missing | absent | clean |
      | different from both copies | present | content_differs |
      | different from the upload copy | absent | content_differs |
      | missing | present | missing_on_server |

  # Covers: server-drift-guard / Drift check classifies each target file / Drift classification 02 - single-file upload of the committed version
  Scenario: Drift classification 02 - single-file upload of the committed version
    Given a single-file upload with `before_changes` set, and a server copy equal to the file at HEAD
    When the tool runs the drift check
    Then the file is clean because the upload copy is the HEAD version

  # Covers: server-drift-guard / Drift check classifies each target file / Drift classification 03 - download error is not treated as missing
  Scenario Outline: Drift classification 03 - download error is not treated as missing
    Given a deployment with one drifted file and one target file whose server download ends with <failure>
    When the tool runs the drift check
    Then the tool returns an error naming the failing file, uploads nothing, and creates no directory

    Examples:
      | failure |
      | a 451 local error |
      | a 550 reply while the parent listing contains the file name |
      | a lost connection |

  # Covers: server-drift-guard / Any drifted file refuses the whole run / Drift refusal 01 - one drifted file blocks the others
  Scenario Outline: Drift refusal 01 - one drifted file blocks the others
    Given a directory or commit deployment with <clean_files> clean target files and one drifted target file
    When a caller runs it with `expect_ref`
    Then the tool uploads no file, creates no directory, reports zero files uploaded and zero directories created, sets `refused` true, and lists exactly the drifted file with its full server path and reason

    Examples:
      | clean_files |
      | 0 |
      | 3 |

  # Covers: server-drift-guard / Any drifted file refuses the whole run / Drift refusal 02 - no drift uploads normally
  Scenario: Drift refusal 02 - no drift uploads normally
    Given a deployment whose target files are all clean
    When a caller runs it with `expect_ref`
    Then the tool uploads the same files and bytes as a run without `expect_ref`, sets `refused` false, and lists no drifted files

  # Covers: server-drift-guard / Any drifted file refuses the whole run / Drift refusal 03 - single-file upload refused
  Scenario: Drift refusal 03 - single-file upload refused
    Given a single-file upload whose server copy differs from both the expected copy and the upload copy
    When a caller runs it with `expect_ref`
    Then the tool leaves the server file unchanged, reports zero bytes uploaded, and lists the file as `content_differs`

  # Covers: server-drift-guard / Dry run with an expected ref checks drift without uploading / Drift dry run 01 - drift is reported without uploading
  Scenario: Drift dry run 01 - drift is reported without uploading
    Given a directory or commit deployment with one drifted target file
    When a caller runs it with `dry_run` set and `expect_ref` supplied
    Then the response lists the planned files and the drifted file, and the server is unchanged

  # Covers: server-drift-guard / Dry run with an expected ref checks drift without uploading / Drift dry run 02 - dry run without expected ref stays offline
  Scenario: Drift dry run 02 - dry run without expected ref stays offline
    Given a directory or commit deployment
    When a caller runs it with `dry_run` set and no `expect_ref`
    Then the tool opens no FTP connection and returns no drift-check result
