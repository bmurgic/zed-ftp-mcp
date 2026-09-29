use super::execute::decide_merge_with;
use super::git::BatchBlobReader;
use super::git::{git_spawn_error, null_device_for_platform};
use super::merge::{self, ConflictReason, MergeDecision};
use super::{
    delete_branch_files_with_connector, deletion_dry_run_manifest, deploy_branch,
    deploy_branch_with_connector, deploy_branch_with_dependencies, dry_run_manifest,
    execute_deletion, execute_deploy, map_remote_path, plan_branch, plan_deletion, BlobSource,
    BranchDeletePlan, BranchDeployError, BranchDeployPlan, BranchRemote, DeleteBranchFilesRequest,
    DeletePathResult, DeletePathStatus, DeletedPathResult, DeletedPathStatus, DeployBranchRequest,
    DeployMode, MergeStatus, PlannedUpload, RemoteComparison, RemoteFailure, UploadStatus,
    UploadedFrom, VerificationStatus,
};
use crate::config::Profile;
use serde_json::json;
use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::rc::Rc;
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteCall {
    Binary,
    Mkdir(String),
    Upload(String, Vec<u8>),
    Compare(String, Vec<u8>),
    #[allow(dead_code)]
    Delete(String),
    Download(String),
}

#[derive(Default)]
struct TestRemote {
    calls: Vec<RemoteCall>,
    failures: std::collections::BTreeMap<usize, RemoteFailure>,
    mismatches: std::collections::BTreeSet<String>,
    comparison_bytes_read: std::collections::BTreeMap<String, u64>,
    /// Server copies by remote path. A path that is not listed here is missing on the server.
    downloads: std::collections::BTreeMap<String, Vec<u8>>,
}

impl TestRemote {
    fn record<T>(&mut self, call: RemoteCall, success: T) -> Result<T, RemoteFailure> {
        self.calls.push(call);
        self.failures
            .get(&self.calls.len())
            .cloned()
            .map_or(Ok(success), Err)
    }
}

impl BranchRemote for TestRemote {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure> {
        self.record(RemoteCall::Binary, ())
    }

    fn mkdir_p(&mut self, path: &str) -> Result<(), RemoteFailure> {
        self.record(RemoteCall::Mkdir(path.to_string()), ())
    }

    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<u64, RemoteFailure> {
        self.record(
            RemoteCall::Upload(path.to_string(), bytes.to_vec()),
            bytes.len() as u64,
        )
    }

    fn compare_remote_bytes(
        &mut self,
        path: &str,
        expected: &[u8],
    ) -> Result<RemoteComparison, RemoteFailure> {
        let matches = !self.mismatches.contains(path);
        let bytes_read = self
            .comparison_bytes_read
            .get(path)
            .copied()
            .unwrap_or(expected.len() as u64);
        self.record(
            RemoteCall::Compare(path.to_string(), expected.to_vec()),
            RemoteComparison {
                matches,
                bytes_read,
            },
        )
    }

    fn delete_file(&mut self, path: &str) -> Result<(), RemoteFailure> {
        self.record(RemoteCall::Delete(path.to_string()), ())
    }

    fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure> {
        let server_copy = self.downloads.get(path).cloned();
        self.record(RemoteCall::Download(path.to_string()), server_copy)
    }
}

#[derive(Default)]
struct TestBlobs {
    blobs: std::collections::BTreeMap<String, Vec<u8>>,
    reads: Vec<String>,
}

impl BlobSource for TestBlobs {
    fn read_blob(&mut self, object_id: &str) -> Result<Vec<u8>, BranchDeployError> {
        self.reads.push(object_id.to_string());
        self.blobs.get(object_id).cloned().ok_or_else(|| {
            BranchDeployError::Other(anyhow::anyhow!("missing test blob {object_id}"))
        })
    }
}

struct CountingBlobSource {
    reads: Rc<Cell<usize>>,
}

impl BlobSource for CountingBlobSource {
    fn read_blob(&mut self, _object_id: &str) -> Result<Vec<u8>, BranchDeployError> {
        self.reads.set(self.reads.get() + 1);
        Err(BranchDeployError::Other(anyhow::anyhow!(
            "dry run must not read blobs"
        )))
    }
}

fn executor_plan() -> BranchDeployPlan {
    let mut plan = BranchDeployPlan::empty("staging", "/repo");
    plan.uploads = vec![
        PlannedUpload {
            git_path: "a.bin".to_string(),
            remote_path: "/remote/a.bin".to_string(),
            object_id: "a".to_string(),
            bytes: 4,
            base_object_id: None,
        },
        PlannedUpload {
            git_path: "nested/b.bin".to_string(),
            remote_path: "/remote/nested/b.bin".to_string(),
            object_id: "b".to_string(),
            bytes: 3,
            base_object_id: None,
        },
    ];
    plan.touched_paths = plan.uploads.len();
    plan
}

fn executor_blobs() -> TestBlobs {
    TestBlobs {
        blobs: std::collections::BTreeMap::from([
            ("a".to_string(), vec![0, b'\r', b'\n', 0xff]),
            ("b".to_string(), vec![0x80, 1, 2]),
        ]),
        ..TestBlobs::default()
    }
}

fn deletion_executor_plan() -> BranchDeletePlan {
    BranchDeletePlan {
        profile: "staging".to_string(),
        repository_root: "/repo".to_string(),
        base_commit: "a".repeat(40),
        head_commit: "b".repeat(40),
        reason: "approved removal".to_string(),
        dry_run: false,
        paths: vec![
            DeletePathResult {
                git_path: "a.txt".to_string(),
                remote_path: "/remote/a.txt".to_string(),
                status: DeletePathStatus::Planned,
            },
            DeletePathResult {
                git_path: "b.txt".to_string(),
                remote_path: "/remote/b.txt".to_string(),
                status: DeletePathStatus::Planned,
            },
        ],
        blocked: Vec::new(),
        failures: Vec::new(),
    }
}

#[test]
fn deletion_preflight_requires_full_pinned_commits_paths_and_reason() {
    let repository = TestRepo::new();
    repository.write("gone.txt", b"before");
    repository.write("also-gone.txt", b"before");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    fs::remove_file(repository.path().join("gone.txt")).expect("fixture file should delete");
    fs::remove_file(repository.path().join("also-gone.txt")).expect("fixture file should delete");
    repository.commit("delete");
    let head = repository.rev_parse("HEAD");

    for request in [
        deletion_request(
            repository.path(),
            &base[..12],
            &head,
            vec!["gone.txt"],
            "reason",
        ),
        deletion_request(
            repository.path(),
            &base,
            &head[..12],
            vec!["gone.txt"],
            "reason",
        ),
        deletion_request(
            repository.path(),
            &"0".repeat(40),
            &head,
            vec!["gone.txt"],
            "reason",
        ),
        deletion_request(repository.path(), &base, &head, Vec::new(), "reason"),
        deletion_request(repository.path(), &base, &head, vec!["gone.txt"], "   "),
        deletion_request(
            repository.path(),
            &base,
            &head,
            vec!["missing.txt"],
            "reason",
        ),
        deletion_request(
            repository.path(),
            &base,
            &head,
            vec!["gone.txt", "gone.txt"],
            "reason",
        ),
    ] {
        let manifest =
            delete_branch_files_with_connector(&request, &test_profile("/remote/root"), |_, _| {
                panic!("invalid preflight must not reach credential or remote execution")
            })
            .expect("preflight should return its complete rejection");
        assert!(!manifest.success);
        assert!(!manifest.blocked.is_empty());
        assert!(manifest.paths.is_empty());
    }

    let plan = plan_deletion(
        &deletion_request(
            repository.path(),
            &base,
            &head,
            vec!["gone.txt", "also-gone.txt"],
            "reason",
        ),
        &test_profile("/remote/root"),
    )
    .expect("full canonical commits should authorize the exact deleted path");
    assert!(plan.blocked.is_empty());
    assert_eq!(
        plan.paths
            .iter()
            .map(|path| path.git_path.as_str())
            .collect::<Vec<_>>(),
        vec!["also-gone.txt", "gone.txt"]
    );
}

#[test]
fn deletion_preflight_rejects_unsafe_non_ascii_and_case_colliding_paths_atomically() {
    let repository = TestRepo::new();
    repository.write("gone.txt", b"before");
    repository.write("Case.txt", b"before");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    fs::remove_file(repository.path().join("gone.txt")).expect("fixture file should delete");
    fs::remove_file(repository.path().join("Case.txt")).expect("fixture file should delete");
    repository.write("case.txt", b"after");
    repository.commit("delete and case rename");
    let head = repository.rev_parse("HEAD");

    let plan = plan_deletion(
        &deletion_request(
            repository.path(),
            &base,
            &head,
            vec!["gone.txt", "../unsafe", "GONE.TXT", "Case.txt", "café.txt"],
            "reason",
        ),
        &test_profile("/remote/root"),
    )
    .expect("preflight should return all blockers");

    assert!(plan.paths.is_empty());
    assert!(plan
        .blocked
        .iter()
        .any(|blocked| blocked.git_path == Some("../unsafe".to_string())));
    assert!(plan
        .blocked
        .iter()
        .any(|blocked| blocked.git_path == Some("GONE.TXT".to_string())));
    assert!(plan
        .blocked
        .iter()
        .any(|blocked| blocked.git_path == Some("Case.txt".to_string())));
    assert!(plan
        .blocked
        .iter()
        .any(|blocked| blocked.git_path == Some("café.txt".to_string())));
}

#[test]
fn deletion_preflight_preserves_requested_dry_run_without_execution() {
    let repository = TestRepo::new();
    repository.write("gone.txt", b"before");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    fs::remove_file(repository.path().join("gone.txt")).expect("fixture file should delete");
    repository.commit("delete");
    let head = repository.rev_parse("HEAD");

    for expected_dry_run in [false, true] {
        let mut request = deletion_request(
            repository.path(),
            &base,
            &head,
            vec!["gone.txt", "not-deleted.txt"],
            "reason",
        );
        request.dry_run = expected_dry_run;
        let manifest =
            delete_branch_files_with_connector(&request, &test_profile("/remote/root"), |_, _| {
                panic!("blocked preflight must not reach credential or FTP execution")
            })
            .expect("blocked preflight should return a manifest");

        assert!(!manifest.success);
        assert_eq!(manifest.dry_run, expected_dry_run);
        assert!(!manifest.blocked.is_empty());
        assert!(manifest.paths.is_empty());
    }
}

#[test]
fn deletion_preflight_dry_run_returns_planned_paths_without_execution() {
    let repository = TestRepo::new();
    repository.write("gone.txt", b"before");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    fs::remove_file(repository.path().join("gone.txt")).expect("fixture file should delete");
    repository.commit("delete");
    let head = repository.rev_parse("HEAD");

    let mut request = deletion_request(
        repository.path(),
        &base,
        &head,
        vec!["gone.txt"],
        "approved removal",
    );
    request.dry_run = true;
    let manifest =
        delete_branch_files_with_connector(&request, &test_profile("/remote/root"), |_, _| {
            panic!("successful dry run must not reach credential or FTP execution")
        })
        .expect("dry run should return its authorized plan");

    assert!(manifest.success);
    assert!(manifest.dry_run);
    assert_eq!(manifest.counts.planned, 1);
    assert_eq!(manifest.paths[0].git_path, "gone.txt");
    assert_eq!(manifest.paths[0].status, DeletePathStatus::Planned);
}

#[test]
fn deletion_executor_dry_run_returns_planned_paths_without_remote_calls() {
    let mut remote = TestRemote::default();
    let mut plan = deletion_executor_plan();
    plan.dry_run = true;

    let manifest = execute_deletion(plan, &mut remote);

    assert!(manifest.success);
    assert!(remote.calls.is_empty());
    assert!(manifest
        .paths
        .iter()
        .all(|path| path.status == DeletePathStatus::Planned));
}

#[test]
fn deletion_executor_uses_binary_mode_and_exact_deterministic_path_order() {
    let mut remote = TestRemote::default();

    let manifest = execute_deletion(deletion_executor_plan(), &mut remote);

    assert!(manifest.success);
    assert_eq!(
        remote.calls,
        vec![
            RemoteCall::Binary,
            RemoteCall::Delete("/remote/a.txt".to_string()),
            RemoteCall::Delete("/remote/b.txt".to_string()),
        ]
    );
    assert!(manifest
        .paths
        .iter()
        .all(|path| path.status == DeletePathStatus::Deleted));
}

#[test]
fn deletion_executor_binary_mode_failure_stops_before_any_delete() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            1,
            RemoteFailure::operation("TYPE I refused"),
        )]),
        ..TestRemote::default()
    };

    let manifest = execute_deletion(deletion_executor_plan(), &mut remote);

    assert!(!manifest.success);
    assert_eq!(remote.calls, vec![RemoteCall::Binary]);
    assert_eq!(manifest.failures.len(), 1);
    assert_eq!(manifest.failures[0].stage, "binary_mode");
    assert_eq!(manifest.failures[0].git_path, None);
    assert_eq!(manifest.failures[0].error, "TYPE I refused");
    assert!(manifest
        .paths
        .iter()
        .all(|path| path.status == DeletePathStatus::NotAttempted));
}

#[test]
fn deletion_executor_operation_failure_continues_but_connection_loss_stops() {
    let mut operation_remote = TestRemote {
        failures: std::collections::BTreeMap::from([(2, RemoteFailure::operation("denied"))]),
        ..TestRemote::default()
    };
    let operation_manifest = execute_deletion(deletion_executor_plan(), &mut operation_remote);
    assert_eq!(operation_manifest.paths[0].status, DeletePathStatus::Failed);
    assert_eq!(
        operation_manifest.paths[1].status,
        DeletePathStatus::Deleted
    );

    let mut lost_remote = TestRemote {
        failures: std::collections::BTreeMap::from([(2, RemoteFailure::connection_lost("lost"))]),
        ..TestRemote::default()
    };
    let lost_manifest = execute_deletion(deletion_executor_plan(), &mut lost_remote);
    assert_eq!(lost_manifest.paths[0].status, DeletePathStatus::Failed);
    assert_eq!(
        lost_manifest.paths[1].status,
        DeletePathStatus::NotAttempted
    );
    assert_eq!(lost_remote.calls.len(), 2);
}

#[test]
fn executor_uploads_binary_bytes_in_git_path_order_and_verifies_immediately() {
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(manifest.success);
    assert_eq!(blobs.reads, vec!["a", "b"]);
    assert_eq!(
        remote.calls,
        vec![
            RemoteCall::Binary,
            RemoteCall::Mkdir("/remote".to_string()),
            RemoteCall::Upload("/remote/a.bin".to_string(), vec![0, b'\r', b'\n', 0xff]),
            RemoteCall::Compare("/remote/a.bin".to_string(), vec![0, b'\r', b'\n', 0xff]),
            RemoteCall::Mkdir("/remote/nested".to_string()),
            RemoteCall::Upload("/remote/nested/b.bin".to_string(), vec![0x80, 1, 2]),
            RemoteCall::Compare("/remote/nested/b.bin".to_string(), vec![0x80, 1, 2]),
        ]
    );
    assert_eq!(manifest.counts.uploaded, 2);
    assert_eq!(manifest.counts.verified, 2);
    assert!(manifest
        .uploads
        .iter()
        .all(|upload| upload.verification_status == VerificationStatus::Verified));
}

#[test]
fn executor_binary_mode_failure_stops_before_data_operations() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            1,
            RemoteFailure::operation("TYPE I refused"),
        )]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert!(blobs.reads.is_empty());
    assert_eq!(
        remote.calls,
        vec![RemoteCall::Binary],
        "binary mode must succeed before any MKD, STOR, or RETR call"
    );
    assert_eq!(manifest.failures.len(), 1);
    assert_eq!(manifest.failures[0].stage, "binary_mode");
    assert_eq!(manifest.failures[0].git_path, None);
    assert_eq!(manifest.failures[0].error, "TYPE I refused");
    assert!(manifest
        .uploads
        .iter()
        .all(|upload| upload.upload_status == UploadStatus::NotAttempted));
    assert!(manifest
        .uploads
        .iter()
        .all(|upload| upload.verification_status == VerificationStatus::NotAttempted));
    assert_eq!(manifest.counts.commits, 0);
    assert_eq!(manifest.counts.touched_paths, 2);
    assert_eq!(manifest.counts.planned_uploads, 2);
    assert_eq!(manifest.counts.uploaded, 0);
    assert_eq!(manifest.counts.verified, 0);
    assert_eq!(manifest.counts.deleted_reported, 0);
    assert_eq!(manifest.counts.failures, 1);
}

#[test]
fn executor_skips_comparison_only_when_disabled() {
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), false, &mut blobs, &mut remote);

    assert!(manifest.success);
    assert!(remote
        .calls
        .iter()
        .all(|call| !matches!(call, RemoteCall::Compare(_, _))));
    assert!(manifest
        .uploads
        .iter()
        .all(|upload| upload.verification_status == VerificationStatus::NotRequested));
}

#[test]
fn executor_batch_blob_reader_returns_exact_committed_bytes() {
    let repository = TestRepo::new();
    let expected = [0, b'\r', b'\n', 0xff, 0x80];
    repository.write("binary.bin", &expected);
    repository.commit("binary fixture");
    let object_id = repository.rev_parse("HEAD:binary.bin");
    let mut blobs = BatchBlobReader::new(repository.path()).expect("batch reader should start");

    let actual = blobs
        .read_blob(&object_id)
        .expect("batch reader should return the committed blob");

    assert_eq!(actual, expected);
}

#[test]
fn executor_batch_blob_reader_reads_two_committed_blobs_sequentially() {
    let repository = TestRepo::new();
    let first_expected = [0, b'\r', b'\n', 0xff, 0x80];
    let second_expected = [0x81, 1, 2, b'\n'];
    repository.write("first.bin", &first_expected);
    repository.write("second.bin", &second_expected);
    repository.commit("binary fixtures");
    let first_object_id = repository.rev_parse("HEAD:first.bin");
    let second_object_id = repository.rev_parse("HEAD:second.bin");
    let mut blobs = BatchBlobReader::new(repository.path()).expect("batch reader should start");

    let first_actual = blobs
        .read_blob(&first_object_id)
        .expect("batch reader should return the first committed blob");
    let second_actual = blobs
        .read_blob(&second_object_id)
        .expect("batch reader should return the second committed blob");

    assert_eq!(first_actual, first_expected);
    assert_eq!(second_actual, second_expected);
}

#[test]
fn executor_operation_upload_failure_continues() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            3,
            RemoteFailure::operation("upload refused"),
        )]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert_eq!(manifest.uploads[0].upload_status, UploadStatus::Failed);
    assert_eq!(
        manifest.uploads[0].verification_status,
        VerificationStatus::NotAttempted
    );
    assert_eq!(manifest.uploads[1].upload_status, UploadStatus::Uploaded);
    assert_eq!(
        manifest.uploads[1].verification_status,
        VerificationStatus::Verified
    );
    assert_eq!(manifest.failures[0].stage, "upload");
    assert_eq!(manifest.failures[0].git_path.as_deref(), Some("a.bin"));
    assert_eq!(manifest.counts.uploaded, 1);
    assert_eq!(manifest.counts.verified, 1);
}

#[test]
fn deployment_executor_reports_removed_paths_without_deleting_them() {
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();
    let mut plan = executor_plan();
    plan.deleted.push(DeletedPathResult {
        git_path: "removed.txt".to_string(),
        status: DeletedPathStatus::RequiresExplicitCall,
    });

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert!(manifest.success);
    assert_eq!(manifest.deleted.len(), 1);
    assert_eq!(
        manifest.deleted[0].status,
        DeletedPathStatus::RequiresExplicitCall
    );
    assert!(
        remote
            .calls
            .iter()
            .all(|call| !matches!(call, RemoteCall::Delete(_))),
        "branch deployment must not delete reported paths"
    );
}

#[test]
fn executor_operation_mkdir_failure_skips_item_and_continues() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(2, RemoteFailure::operation("MKD refused"))]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert_eq!(manifest.uploads[0].upload_status, UploadStatus::Failed);
    assert_eq!(
        manifest.uploads[0].verification_status,
        VerificationStatus::NotAttempted
    );
    assert_eq!(manifest.uploads[1].upload_status, UploadStatus::Uploaded);
    assert_eq!(
        manifest.uploads[1].verification_status,
        VerificationStatus::Verified
    );
    assert_eq!(manifest.failures[0].stage, "mkdir");
    assert_eq!(manifest.failures[0].git_path.as_deref(), Some("a.bin"));
    assert_eq!(
        remote.calls,
        vec![
            RemoteCall::Binary,
            RemoteCall::Mkdir("/remote".to_string()),
            RemoteCall::Mkdir("/remote/nested".to_string()),
            RemoteCall::Upload("/remote/nested/b.bin".to_string(), vec![0x80, 1, 2]),
            RemoteCall::Compare("/remote/nested/b.bin".to_string(), vec![0x80, 1, 2]),
        ]
    );
}

#[test]
fn executor_connection_loss_during_mkdir_stops_without_uploading_or_reconnecting() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            2,
            RemoteFailure::connection_lost("MKD connection lost"),
        )]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert_eq!(manifest.uploads[0].upload_status, UploadStatus::Failed);
    assert_eq!(
        manifest.uploads[0].verification_status,
        VerificationStatus::NotAttempted
    );
    assert_eq!(
        manifest.uploads[1].upload_status,
        UploadStatus::NotAttempted
    );
    assert_eq!(
        manifest.uploads[1].verification_status,
        VerificationStatus::NotAttempted
    );
    assert_eq!(manifest.failures[0].stage, "mkdir");
    assert_eq!(manifest.failures[0].git_path.as_deref(), Some("a.bin"));
    assert_eq!(blobs.reads, vec!["a"]);
    assert_eq!(
        remote.calls,
        vec![RemoteCall::Binary, RemoteCall::Mkdir("/remote".to_string()),]
    );
}

#[test]
fn executor_operation_compare_failure_continues() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(4, RemoteFailure::operation("RETR refused"))]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert_eq!(manifest.uploads[0].upload_status, UploadStatus::Uploaded);
    assert_eq!(
        manifest.uploads[0].verification_status,
        VerificationStatus::Failed
    );
    assert_eq!(
        manifest.uploads[1].verification_status,
        VerificationStatus::Verified
    );
    assert_eq!(manifest.failures[0].stage, "verification");
    assert_eq!(manifest.counts.uploaded, 2);
    assert_eq!(manifest.counts.verified, 1);
}

#[test]
fn executor_verification_mismatch_drains_and_fails_manifest() {
    let mut remote = TestRemote {
        mismatches: std::collections::BTreeSet::from(["/remote/a.bin".to_string()]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert_eq!(
        manifest.uploads[0].verification_status,
        VerificationStatus::Mismatch
    );
    assert_eq!(
        manifest.uploads[1].verification_status,
        VerificationStatus::Verified
    );
    assert_eq!(manifest.failures[0].stage, "verification");
    assert_eq!(manifest.counts.verified, 1);
}

#[test]
fn executor_records_measured_remote_bytes_for_verified_and_mismatched_uploads() {
    let mut remote = TestRemote {
        comparison_bytes_read: std::collections::BTreeMap::from([
            ("/remote/a.bin".to_string(), 11),
            ("/remote/nested/b.bin".to_string(), 13),
        ]),
        mismatches: std::collections::BTreeSet::from(["/remote/a.bin".to_string()]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);
    let serialized = serde_json::to_value(manifest).expect("manifest should serialize");

    assert_eq!(
        serialized.pointer("/uploads/0/remote_bytes_read"),
        Some(&json!(11))
    );
    assert_eq!(
        serialized.pointer("/uploads/1/remote_bytes_read"),
        Some(&json!(13))
    );
}

#[test]
fn executor_omits_remote_bytes_without_a_completed_comparison() {
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();
    let verification_disabled = execute_deploy(executor_plan(), false, &mut blobs, &mut remote);
    let planned = dry_run_manifest(executor_plan(), true);

    let mut failed_comparison_remote = TestRemote {
        failures: std::collections::BTreeMap::from([(4, RemoteFailure::operation("RETR refused"))]),
        ..TestRemote::default()
    };
    let mut failed_comparison_blobs = executor_blobs();
    let comparison_failed = execute_deploy(
        executor_plan(),
        true,
        &mut failed_comparison_blobs,
        &mut failed_comparison_remote,
    );
    let mut not_attempted_remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            2,
            RemoteFailure::connection_lost("MKD connection lost"),
        )]),
        ..TestRemote::default()
    };
    let mut not_attempted_blobs = executor_blobs();
    let not_attempted = execute_deploy(
        executor_plan(),
        true,
        &mut not_attempted_blobs,
        &mut not_attempted_remote,
    );

    for (manifest, upload_index) in [
        (verification_disabled, 0),
        (planned, 0),
        (comparison_failed, 0),
        (not_attempted, 1),
    ] {
        let serialized = serde_json::to_value(manifest).expect("manifest should serialize");
        assert!(
            serialized
                .pointer(&format!("/uploads/{upload_index}/remote_bytes_read"))
                .is_none(),
            "remote byte count should be absent before comparison completes"
        );
    }
}

#[test]
fn executor_connection_loss_marks_remaining_not_attempted() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            3,
            RemoteFailure::connection_lost("FTP connection lost"),
        )]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(!manifest.success);
    assert_eq!(manifest.uploads[0].upload_status, UploadStatus::Failed);
    assert_eq!(
        manifest.uploads[1].upload_status,
        UploadStatus::NotAttempted
    );
    assert_eq!(
        manifest.uploads[1].verification_status,
        VerificationStatus::NotAttempted
    );
    assert_eq!(manifest.failures[0].stage, "upload");
    assert_eq!(manifest.counts.uploaded, 0);
    assert_eq!(manifest.counts.verified, 0);
}

#[test]
fn executor_never_reconnects_after_connection_loss() {
    let mut remote = TestRemote {
        failures: std::collections::BTreeMap::from([(
            4,
            RemoteFailure::connection_lost("RETR connection lost"),
        )]),
        ..TestRemote::default()
    };
    let mut blobs = executor_blobs();

    let _manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert_eq!(
        remote.calls,
        vec![
            RemoteCall::Binary,
            RemoteCall::Mkdir("/remote".to_string()),
            RemoteCall::Upload("/remote/a.bin".to_string(), vec![0, b'\r', b'\n', 0xff]),
            RemoteCall::Compare("/remote/a.bin".to_string(), vec![0, b'\r', b'\n', 0xff]),
        ]
    );
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

#[test]
fn planner_contract_serializes_stable_manifest_fields() {
    let manifest = dry_run_manifest(BranchDeployPlan::empty("staging", "/repo"), true);
    let value = serde_json::to_value(manifest).expect("manifest should serialize");

    assert_eq!(
        value
            .as_object()
            .expect("manifest should be an object")
            .keys()
            .collect::<Vec<_>>(),
        vec![
            "blocked_by_conflicts",
            "counts",
            "deleted",
            "dry_run",
            "failures",
            "merge_rule",
            "mode",
            "profile",
            "refs",
            "repository",
            "success",
            "uploads",
            "verify",
        ]
    );
    for (status, expected) in [
        (UploadStatus::Planned, "planned"),
        (UploadStatus::Uploaded, "uploaded"),
        (UploadStatus::Failed, "failed"),
        (UploadStatus::NotAttempted, "not_attempted"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(expected));
    }
    for (status, expected) in [
        (VerificationStatus::Planned, "planned"),
        (VerificationStatus::Verified, "verified"),
        (VerificationStatus::Mismatch, "mismatch"),
        (VerificationStatus::NotRequested, "not_requested"),
        (VerificationStatus::Failed, "failed"),
        (VerificationStatus::NotAttempted, "not_attempted"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(expected));
    }
    for (status, expected) in [
        (
            DeletedPathStatus::RequiresExplicitCall,
            "requires_explicit_call",
        ),
        (
            DeletedPathStatus::BlockedCaseCollision,
            "blocked_case_collision",
        ),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(expected));
    }
}

#[test]
fn branch_range_04_mode_defaults_to_overwrite() {
    assert_eq!(DeployMode::default(), DeployMode::Overwrite);

    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"head");
    repository.commit("head");
    let plan = plan_for(&repository, &base);
    assert_eq!(plan.mode, DeployMode::Overwrite);

    let value = serde_json::to_value(dry_run_manifest(plan, true)).expect("manifest serializes");

    assert_eq!(value["mode"], json!("overwrite"));
    assert_eq!(value["blocked_by_conflicts"], json!(false));
    assert!(value.pointer("/uploads/0/merge_status").is_none());
}

#[test]
fn deploy_mode_serializes_as_lowercase_words() {
    assert_eq!(
        serde_json::to_value(DeployMode::Overwrite).unwrap(),
        json!("overwrite")
    );
    assert_eq!(
        serde_json::to_value(DeployMode::Merge).unwrap(),
        json!("merge")
    );
}

#[test]
fn deletion_manifest_reports_only_the_repository_root() {
    let manifest = deletion_dry_run_manifest(BranchDeletePlan {
        profile: "staging".to_string(),
        repository_root: "/approved/repository".to_string(),
        base_commit: "a".repeat(40),
        head_commit: "b".repeat(40),
        reason: "approved cleanup".to_string(),
        dry_run: true,
        paths: Vec::new(),
        blocked: Vec::new(),
        failures: Vec::new(),
    });
    let manifest = serde_json::to_value(manifest).expect("deletion manifest should serialize");

    assert_eq!(
        manifest.pointer("/repository_root"),
        Some(&json!("/approved/repository"))
    );
    assert!(manifest.pointer("/repository").is_none());
}

#[test]
fn planner_path_maps_under_remote_root() {
    let (git_path, remote_path) = map_remote_path("/remote/root/", b"assets/app.js")
        .expect("path should map under remote root");

    assert_eq!(git_path, "assets/app.js");
    assert_eq!(remote_path, "/remote/root/assets/app.js");
}

#[test]
fn planner_path_rejects_absolute_dot_backslash_and_control_components() {
    for path in [
        b"/absolute".as_slice(),
        b"./dot",
        b"dir/../escape",
        b"dir\\file",
        b"dir/\x01file",
        b"dir//file",
    ] {
        assert!(
            map_remote_path("/remote/root", path).is_err(),
            "{path:?} should fail"
        );
    }
}

#[cfg(unix)]
#[test]
fn planner_path_rejects_non_utf8() {
    assert!(map_remote_path("/remote/root", b"invalid-\xff").is_err());
}

#[test]
fn planner_config_uses_platform_null_devices() {
    assert_eq!(null_device_for_platform(true), "NUL");
    assert_eq!(null_device_for_platform(false), "/dev/null");
}

#[test]
fn planner_rejects_relative_subdirectory_and_missing_ref() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    fs::create_dir(repository.path().join("subdirectory")).expect("subdirectory should exist");

    let profile = test_profile("/remote/root");
    let relative = request(".", "HEAD", "HEAD");
    assert_invalid(&relative, &profile);

    let subdirectory = request(
        repository
            .path()
            .join("subdirectory")
            .to_str()
            .expect("utf-8 path"),
        "HEAD",
        "HEAD",
    );
    assert_invalid(&subdirectory, &profile);

    let missing_ref = request(
        repository.path().to_str().expect("utf-8 path"),
        "missing-ref",
        "HEAD",
    );
    assert_invalid(&missing_ref, &profile);

    let linked_worktree = TempDir::new().expect("linked-worktree parent should exist");
    let linked_root = linked_worktree.path().join("linked");
    repository.git_success(&[
        "worktree",
        "add",
        "--detach",
        linked_root.to_str().expect("utf-8 path"),
        "HEAD",
    ]);
    let linked_plan = plan_branch(
        &request(linked_root.to_str().expect("utf-8 path"), "HEAD", "HEAD"),
        &profile,
    )
    .expect("linked worktree root should be accepted");
    assert_eq!(
        linked_plan.repository.root,
        linked_root
            .canonicalize()
            .expect("linked worktree root should resolve")
            .display()
            .to_string()
    );
}

#[test]
fn planner_defaults_head_to_head_and_records_full_ids() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"head");
    repository.commit("head");
    let head = repository.rev_parse("HEAD");

    let plan = plan_branch(
        &request(
            repository.path().to_str().expect("utf-8 path"),
            &base,
            "HEAD",
        ),
        &test_profile("/remote/root"),
    )
    .expect("planner should resolve HEAD");

    assert_eq!(plan.refs.base.requested, base);
    assert_eq!(plan.refs.base.commit, base);
    assert_eq!(plan.refs.head.requested, "HEAD");
    assert_eq!(plan.refs.head.commit, head);
    assert_eq!(plan.refs.head.commit.len(), 40);
}

#[test]
fn planner_unions_intermediate_repeated_and_restored_paths() {
    let repository = TestRepo::new();
    repository.write("restored.txt", b"original");
    repository.write("repeated.txt", b"zero");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");

    repository.write("restored.txt", b"temporary");
    repository.write("repeated.txt", b"one");
    repository.commit("first change");
    repository.write("restored.txt", b"original");
    repository.write("repeated.txt", b"two");
    repository.commit("restore and repeat");

    let plan = plan_for(&repository, &base);
    let paths: Vec<&str> = plan
        .uploads
        .iter()
        .map(|upload| upload.git_path.as_str())
        .collect();
    assert_eq!(paths, vec!["repeated.txt", "restored.txt"]);
    assert_eq!(
        upload(&plan, "restored.txt").object_id,
        repository.git_text(&["hash-object", "restored.txt"])
    );
    assert_eq!(
        upload(&plan, "repeated.txt").object_id,
        repository.git_text(&["hash-object", "repeated.txt"])
    );
}

#[test]
fn planner_keeps_merged_side_commits_and_first_parent_resolution() {
    let repository = TestRepo::new();
    repository.write("shared.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");

    repository.git_success(&["checkout", "-b", "side"]);
    repository.write("side.txt", b"side");
    repository.write("shared.txt", b"side");
    repository.commit("side change");

    repository.git_success(&["checkout", "main"]);
    repository.write("main.txt", b"main");
    repository.write("shared.txt", b"main");
    repository.commit("main change");
    let merge = repository.git(&["merge", "side", "--no-ff", "--no-commit"]);
    assert!(
        !merge.status.success(),
        "merge should conflict for the fixture"
    );
    repository.write("shared.txt", b"resolved");
    repository.write("resolution.txt", b"merge only");
    repository.git_success(&["add", "shared.txt", "resolution.txt", "side.txt"]);
    repository.git_success(&["commit", "-m", "resolve merge"]);

    let plan = plan_for(&repository, &base);
    let paths: Vec<&str> = plan
        .uploads
        .iter()
        .map(|upload| upload.git_path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["main.txt", "resolution.txt", "shared.txt", "side.txt"]
    );
    assert_eq!(
        upload(&plan, "resolution.txt").object_id,
        repository.rev_parse("HEAD:resolution.txt")
    );
}

#[test]
fn planner_uses_dirty_head_blob_metadata_and_ignores_profile_filters() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"committed\0bytes");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"committed\0bytes changed");
    repository.commit("head");
    repository.write("tracked.txt", b"working tree bytes that must not be read");

    let mut profile = test_profile("/remote/root");
    profile.local_root = "/not/the/selected/worktree".to_string();
    profile.ignore = vec!["tracked.txt".to_string()];
    let plan = plan_branch(
        &request(
            repository.path().to_str().expect("utf-8 path"),
            &base,
            "HEAD",
        ),
        &profile,
    )
    .expect("planner should ignore local root and filters");

    let expected_object = repository.rev_parse("HEAD:tracked.txt");
    let expected_bytes: u64 = repository
        .git_text(&["cat-file", "-s", "HEAD:tracked.txt"])
        .parse()
        .expect("git should report blob size");
    let planned = upload(&plan, "tracked.txt");
    assert!(plan.repository.dirty);
    assert_eq!(planned.object_id, expected_object);
    assert_eq!(planned.bytes, expected_bytes);
}

#[test]
fn planner_reports_deleted_recreated_submodule_and_unsafe_entries() {
    let repository = TestRepo::new();
    repository.write("Case.txt", b"original case");
    repository.write("deleted.txt", b"deleted");
    repository.write("recreated.txt", b"old");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");

    fs::remove_file(repository.path().join("deleted.txt")).expect("delete fixture path");
    fs::remove_file(repository.path().join("recreated.txt")).expect("delete recreated path");
    fs::remove_file(repository.path().join("Case.txt")).expect("delete case fixture path");
    repository.commit("remove files");
    repository.write("case.txt", b"replacement case");
    repository.write("recreated.txt", b"new");
    repository.write("unsafe\\name.txt", b"unsafe");
    repository.git_success(&["add", "-A"]);
    #[cfg(unix)]
    repository.add_non_utf8_blob(
        &repository.rev_parse("HEAD~1:recreated.txt"),
        b"invalid-\xff",
    );
    let commit = repository.rev_parse("HEAD");
    repository.git_success(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{commit},submodule"),
    ]);
    repository.git_success(&["commit", "-m", "recreate and add unusual entries"]);

    let plan = plan_for(&repository, &base);
    assert_eq!(
        plan.deleted
            .iter()
            .find(|entry| entry.git_path == "deleted.txt")
            .expect("deleted path should be reported")
            .status,
        DeletedPathStatus::RequiresExplicitCall
    );
    assert_eq!(
        plan.deleted
            .iter()
            .find(|entry| entry.git_path == "Case.txt")
            .expect("case-only rename source should be reported")
            .status,
        DeletedPathStatus::BlockedCaseCollision
    );
    assert_eq!(
        upload(&plan, "recreated.txt").object_id,
        repository.rev_parse("HEAD:recreated.txt")
    );
    assert!(plan
        .uploads
        .iter()
        .all(|entry| entry.git_path != "submodule"));
    assert!(plan
        .uploads
        .iter()
        .all(|entry| entry.git_path != "unsafe\\name.txt"));
    assert!(plan
        .failures
        .iter()
        .any(|failure| failure.git_path.as_deref() == Some("submodule")));
    assert!(plan
        .failures
        .iter()
        .any(|failure| failure.git_path.as_deref() == Some("unsafe\\name.txt")));
    #[cfg(unix)]
    assert!(plan
        .failures
        .iter()
        .any(|failure| failure.git_path.is_none()));
}

#[test]
fn planner_reports_deleted_paths_without_upload_work() {
    let repository = TestRepo::new();
    repository.write("deleted.txt", b"deleted");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    fs::remove_file(repository.path().join("deleted.txt")).expect("delete fixture path");
    repository.commit("delete");

    let plan = plan_for(&repository, &base);

    assert_eq!(plan.deleted.len(), 1);
    assert_eq!(plan.deleted[0].git_path, "deleted.txt");
    assert_eq!(
        plan.deleted[0].status,
        DeletedPathStatus::RequiresExplicitCall
    );
    assert!(plan.uploads.is_empty());
}

#[cfg(unix)]
#[test]
fn planner_rejects_symlink_blob_entries() {
    let repository = TestRepo::new();
    repository.write("target.txt", b"target");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    std::os::unix::fs::symlink("target.txt", repository.path().join("linked.txt"))
        .expect("symlink fixture should exist");
    repository.commit("add symlink");

    let plan = plan_for(&repository, &base);

    assert!(plan
        .uploads
        .iter()
        .all(|upload| upload.git_path != "linked.txt"));
    assert!(plan.failures.iter().any(|failure| {
        failure.git_path.as_deref() == Some("linked.txt")
            && failure.error == "head entry is not a deployable regular blob: 120000 blob"
    }));
}

#[cfg(unix)]
#[test]
fn planner_accepts_executable_regular_blob_entries() {
    let repository = TestRepo::new();
    repository.write("base.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("script.sh", b"#!/bin/sh\nprintf executable\n");
    repository.make_executable("script.sh");
    repository.commit("add executable script");

    let plan = plan_for(&repository, &base);

    assert_eq!(
        upload(&plan, "script.sh").object_id,
        repository.rev_parse("HEAD:script.sh")
    );
}

#[test]
fn planner_orders_exact_git_paths_deterministically() {
    let repository = TestRepo::new();
    repository.write("base.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("z.txt", b"z");
    repository.write("a.txt", b"a");
    repository.write("dir/b.txt", b"b");
    repository.commit("unordered names");

    let first = plan_for(&repository, &base);
    let second = plan_for(&repository, &base);
    let first_paths: Vec<&str> = first
        .uploads
        .iter()
        .map(|upload| upload.git_path.as_str())
        .collect();
    let second_paths: Vec<&str> = second
        .uploads
        .iter()
        .map(|upload| upload.git_path.as_str())
        .collect();
    assert_eq!(first_paths, vec!["a.txt", "dir/b.txt", "z.txt"]);
    assert_eq!(first_paths, second_paths);
}

#[test]
fn dry_run_reads_metadata_only() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"head");
    repository.commit("head");

    let request = request(
        repository.path().to_str().expect("utf-8 path"),
        &base,
        "HEAD",
    );
    let profile = test_profile("/remote/root");
    let blob_source_factory_calls = Cell::new(0);
    let blob_reads = Rc::new(Cell::new(0));
    let connector_calls = Cell::new(0);
    let source_reads = Rc::clone(&blob_reads);
    let manifest = deploy_branch_with_dependencies(
        &request,
        &profile,
        |_| {
            blob_source_factory_calls.set(blob_source_factory_calls.get() + 1);
            Ok(CountingBlobSource {
                reads: Rc::clone(&source_reads),
            })
        },
        |_, _| {
            connector_calls.set(connector_calls.get() + 1);
            Err(anyhow::anyhow!("dry run must not connect"))
        },
    )
    .expect("dry run should succeed");

    assert_eq!(blob_source_factory_calls.get(), 0);
    assert_eq!(blob_reads.get(), 0);
    assert_eq!(connector_calls.get(), 0);
    assert_eq!(
        manifest
            .uploads
            .iter()
            .map(|upload| upload.upload_status)
            .collect::<Vec<_>>(),
        vec![UploadStatus::Planned; manifest.uploads.len()]
    );
}

#[test]
fn connection_failure_returns_not_attempted_manifest() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("added.txt", b"added");
    repository.write("tracked.txt", b"head");
    repository.commit("head");

    let mut request = request(
        repository.path().to_str().expect("utf-8 path"),
        &base,
        "HEAD",
    );
    request.dry_run = false;
    let profile = test_profile("/remote/root");

    let mut connector_calls = 0;
    let public_manifest =
        deploy_branch_with_connector(&request, &profile, |profile_name, connected_profile| {
            connector_calls += 1;
            assert_eq!(profile_name, "staging");
            assert_eq!(connected_profile.host, "example.test");
            Err(anyhow::anyhow!("test connection failure"))
        })
        .expect("connection failures should return a complete deployment manifest");

    assert_eq!(connector_calls, 1);
    assert!(!public_manifest.success);
    assert!(public_manifest.verify);
    assert_eq!(public_manifest.profile, "staging");
    assert_eq!(public_manifest.refs.base.commit, base);
    assert_eq!(
        public_manifest.refs.head.commit,
        repository.rev_parse("HEAD")
    );
    assert_eq!(public_manifest.failures[0].stage, "connect");
    assert_eq!(
        public_manifest
            .uploads
            .iter()
            .map(|upload| upload.git_path.as_str())
            .collect::<Vec<_>>(),
        vec!["added.txt", "tracked.txt"]
    );
    assert!(public_manifest.uploads.iter().all(|upload| {
        upload.upload_status == UploadStatus::NotAttempted
            && upload.verification_status == VerificationStatus::NotAttempted
    }));
}

#[test]
fn merge_connection_failure_blocks_the_run_and_leaves_files_undecided() {
    let repository = TestRepo::new();
    repository.write("restored.txt", b"original");
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("restored.txt", b"temporary");
    repository.write("tracked.txt", b"head");
    repository.commit("change");
    repository.write("restored.txt", b"original");
    repository.commit("restore");
    let mut request = request(
        repository.path().to_str().expect("utf-8 path"),
        &base,
        "HEAD",
    );
    request.dry_run = false;
    request.mode = DeployMode::Merge;

    let manifest = deploy_branch_with_connector(&request, &test_profile("/remote/root"), |_, _| {
        Err(anyhow::anyhow!("test connection failure"))
    })
    .expect("connection failures should return a complete deployment manifest");

    assert_eq!(manifest.mode, DeployMode::Merge);
    assert!(manifest.blocked_by_conflicts);
    assert!(!manifest.success);
    assert_eq!(manifest.failures[0].stage, "connect");
    let restored = result_for(&manifest, "restored.txt");
    assert_eq!(restored.merge_status, Some(MergeStatus::UnchangedInRange));
    assert_eq!(restored.upload_status, UploadStatus::NotNeeded);
    let tracked = result_for(&manifest, "tracked.txt");
    assert_eq!(tracked.merge_status, Some(MergeStatus::NotDecided));
    assert_eq!(tracked.upload_status, UploadStatus::NotAttempted);
    assert_eq!(
        tracked.verification_status,
        VerificationStatus::NotAttempted
    );
}

#[cfg(unix)]
#[test]
fn dry_run_does_not_run_repository_fsmonitor() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"head");
    repository.commit("head");
    let sentinel = repository.enable_fsmonitor_sentinel();

    let manifest = deploy_branch(
        &request(
            repository.path().to_str().expect("utf-8 path"),
            &base,
            "HEAD",
        ),
        &test_profile("/remote/root"),
    )
    .expect("dry run should succeed without running the fsmonitor command");

    assert!(manifest.dry_run);
    assert!(
        !sentinel.exists(),
        "dry-run planning must not execute repository-configured fsmonitor commands"
    );
}

#[test]
fn planner_keeps_raw_head_tree_when_a_replacement_ref_exists() {
    let repository = TestRepo::new();
    repository.write("base.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("legitimate.txt", b"legitimate");
    repository.commit("legitimate head");
    let head = repository.rev_parse("HEAD");
    let legitimate_blob = repository.rev_parse("HEAD:legitimate.txt");

    repository.git_success(&["checkout", "-b", "replacement", &base]);
    repository.write("replacement.txt", b"replacement");
    repository.commit("replacement head");
    let replacement = repository.rev_parse("HEAD");
    repository.git_success(&["replace", &head, &replacement]);

    let plan = plan_branch(
        &request(
            repository.path().to_str().expect("utf-8 path"),
            &base,
            &head,
        ),
        &test_profile("/remote/root"),
    )
    .expect("planner must use raw commit identities");

    assert_eq!(plan.refs.head.commit, head);
    assert_eq!(upload(&plan, "legitimate.txt").object_id, legitimate_blob);
    assert!(plan
        .uploads
        .iter()
        .all(|planned_upload| planned_upload.git_path != "replacement.txt"));
}

#[cfg(unix)]
#[test]
fn dry_run_rejects_blobless_partial_clone_without_remote_contact() {
    let source = TestRepo::new();
    source.write("tracked.txt", b"base");
    source.commit("base");
    let base = source.rev_parse("HEAD");
    source.write("tracked.txt", b"head");
    source.commit("head");
    let head_blob = source.rev_parse("HEAD:tracked.txt");
    source.git_success(&["config", "uploadpack.allowFilter", "true"]);

    let clone_parent = TempDir::new().expect("partial-clone parent should exist");
    let clone_root = clone_parent.path().join("partial-clone");
    git_success_at(
        clone_parent.path(),
        &[
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            &format!("file://{}", source.path().display()),
            clone_root.to_str().expect("utf-8 path"),
        ],
    );
    let remote_contact_log = clone_parent.path().join("remote-contact.log");
    let upload_pack = clone_parent.path().join("upload-pack-sentinel.sh");
    fs::write(
        &upload_pack,
        format!(
            "#!/bin/sh\nprintf remote-contact >> '{}'\nexec git-upload-pack \"$@\"\n",
            remote_contact_log.display()
        ),
    )
    .expect("upload-pack sentinel should be written");
    let mut permissions = fs::metadata(&upload_pack)
        .expect("upload-pack sentinel metadata should be readable")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&upload_pack, permissions)
        .expect("upload-pack sentinel should be executable");
    git_success_at(
        &clone_root,
        &[
            "config",
            "remote.origin.uploadpack",
            upload_pack.to_str().expect("utf-8 path"),
        ],
    );

    assert_missing_blob(&clone_root, &head_blob);
    let error = deploy_branch(
        &request(clone_root.to_str().expect("utf-8 path"), &base, "HEAD"),
        &test_profile("/remote/root"),
    )
    .expect_err("dry-run planning must reject blobless partial clones before object inspection");

    assert!(error.to_string().contains("partial/promisor"));
    assert_missing_blob(&clone_root, &head_blob);
    assert!(
        !remote_contact_log.exists(),
        "dry-run planning must not contact a promisor remote"
    );
}

#[cfg(unix)]
#[test]
fn dry_run_rejects_git_true_promisor_spellings_without_object_or_remote_access() {
    let source = TestRepo::new();
    source.write("tracked.txt", b"base");
    source.commit("base");
    let base = source.rev_parse("HEAD");
    source.write("tracked.txt", b"head");
    source.commit("head");
    let head_blob = source.rev_parse("HEAD:tracked.txt");
    source.git_success(&["config", "uploadpack.allowFilter", "true"]);

    for spelling in ["yes", "on", "1"] {
        let clone_parent = TempDir::new().expect("partial-clone parent should exist");
        let clone_root = clone_parent
            .path()
            .join(format!("partial-clone-{spelling}"));
        git_success_at(
            clone_parent.path(),
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                &format!("file://{}", source.path().display()),
                clone_root.to_str().expect("utf-8 path"),
            ],
        );
        git_success_at(&clone_root, &["config", "remote.origin.promisor", spelling]);
        let remote_contact_log = clone_parent.path().join("remote-contact.log");
        let upload_pack = clone_parent.path().join("upload-pack-sentinel.sh");
        fs::write(
            &upload_pack,
            format!(
                "#!/bin/sh\nprintf remote-contact >> '{}'\nexec git-upload-pack \"$@\"\n",
                remote_contact_log.display()
            ),
        )
        .expect("upload-pack sentinel should be written");
        let mut permissions = fs::metadata(&upload_pack)
            .expect("upload-pack sentinel metadata should be readable")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&upload_pack, permissions)
            .expect("upload-pack sentinel should be executable");
        git_success_at(
            &clone_root,
            &[
                "config",
                "remote.origin.uploadpack",
                upload_pack.to_str().expect("utf-8 path"),
            ],
        );

        assert_missing_blob(&clone_root, &head_blob);
        let error = match deploy_branch(
            &request(clone_root.to_str().expect("utf-8 path"), &base, "HEAD"),
            &test_profile("/remote/root"),
        ) {
            Ok(_) => {
                panic!("Git boolean spelling '{spelling}' must reject promisor repositories")
            }
            Err(error) => error,
        };

        assert!(error.to_string().contains("partial/promisor"));
        assert_missing_blob(&clone_root, &head_blob);
        assert!(
            !remote_contact_log.exists(),
            "Git boolean spelling '{spelling}' must reject before remote contact"
        );
    }
}

#[test]
fn dry_run_accepts_git_false_promisor_spellings() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"head");
    repository.commit("head");

    for spelling in ["false", "no", "off", "0"] {
        repository.git_success(&["config", "remote.origin.promisor", spelling]);
        deploy_branch(
            &request(
                repository.path().to_str().expect("utf-8 path"),
                &base,
                "HEAD",
            ),
            &test_profile("/remote/root"),
        )
        .unwrap_or_else(|error| {
            panic!("Git boolean spelling '{spelling}' must not mark a repository promisor: {error}")
        });
    }
}

#[test]
fn dry_run_rejects_invalid_promisor_boolean() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    repository.write("tracked.txt", b"head");
    repository.commit("head");
    repository.git_success(&["config", "remote.origin.promisor", "invalid"]);

    let error = deploy_branch(
        &request(
            repository.path().to_str().expect("utf-8 path"),
            "missing-ref",
            "HEAD",
        ),
        &test_profile("/remote/root"),
    )
    .expect_err("invalid Git booleans must fail closed");

    assert!(error.to_string().contains("boolean"));
}

#[test]
fn planner_reports_same_length_tracked_worktree_change_as_dirty() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", b"base");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("tracked.txt", b"head");
    repository.commit("head");
    repository.write("tracked.txt", b"work");

    let plan = plan_for(&repository, &base);

    assert!(plan.repository.dirty);
}

const MERGE_BASE: &[u8] = b"line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";

fn merge_lines(replacements: &[(usize, &str)]) -> Vec<u8> {
    let mut lines: Vec<String> = (1..=6).map(|number| format!("line {number}")).collect();
    for (line_number, text) in replacements {
        lines[line_number - 1] = (*text).to_string();
    }
    format!("{}\n", lines.join("\n")).into_bytes()
}

fn assert_conflict(decision: MergeDecision, expected_reason: ConflictReason) -> Option<Vec<u8>> {
    match decision {
        MergeDecision::Conflict {
            reason,
            marked_text,
        } => {
            assert_eq!(reason, expected_reason);
            marked_text
        }
        other => panic!("expected a {expected_reason:?} conflict, got {other:?}"),
    }
}

#[test]
fn merge_decide_applies_rules_2_to_8_in_order() {
    let head = merge_lines(&[(2, "line 2 head")]);
    let server_other_line = merge_lines(&[(5, "line 5 server")]);
    let server_same_line = merge_lines(&[(2, "line 2 server")]);
    let binary = b"a\0b".to_vec();
    let binary_head = b"a\0c".to_vec();

    // Rule 2 wins over every later rule, including the binary refusal.
    assert_eq!(
        merge::decide(Some(MERGE_BASE), &head, Some(&head)).unwrap(),
        MergeDecision::AlreadyDeployed
    );
    assert_eq!(
        merge::decide(None, &head, Some(&head)).unwrap(),
        MergeDecision::AlreadyDeployed
    );
    assert_eq!(
        merge::decide(Some(&binary), &binary_head, Some(&binary_head)).unwrap(),
        MergeDecision::AlreadyDeployed
    );
    // Rule 3 wins over the binary refusal, so a binary file fast-forwards.
    assert_eq!(
        merge::decide(Some(MERGE_BASE), &head, Some(MERGE_BASE)).unwrap(),
        MergeDecision::FastForward
    );
    assert_eq!(
        merge::decide(Some(&binary), &binary_head, Some(&binary)).unwrap(),
        MergeDecision::FastForward
    );
    // Rule 4.
    assert_eq!(
        merge::decide(Some(MERGE_BASE), &head, Some(&server_other_line)).unwrap(),
        MergeDecision::Merged(merge_lines(&[(2, "line 2 head"), (5, "line 5 server")]))
    );
    assert_conflict(
        merge::decide(Some(MERGE_BASE), &head, Some(&server_same_line)).unwrap(),
        ConflictReason::TextConflict,
    );
    // Rule 5.
    assert_eq!(
        assert_conflict(
            merge::decide(Some(&binary), &binary_head, Some(b"a\0d")).unwrap(),
            ConflictReason::BinaryChanged
        ),
        None
    );
    // Rule 6.
    assert_eq!(
        assert_conflict(
            merge::decide(Some(MERGE_BASE), &head, None).unwrap(),
            ConflictReason::DeletedOnServer
        ),
        None
    );
    // Rule 7. A symbolic-link base arrives as an absent base.
    assert_eq!(
        merge::decide(None, &head, None).unwrap(),
        MergeDecision::NewFile
    );
    // Rule 8.
    assert_eq!(
        assert_conflict(
            merge::decide(None, &head, Some(&server_other_line)).unwrap(),
            ConflictReason::AddedOnBoth
        ),
        None
    );
}

#[test]
fn merge_decide_refuses_a_nul_byte_in_any_single_version() {
    let text_head = merge_lines(&[(2, "line 2 head")]);
    let text_server = merge_lines(&[(5, "line 5 server")]);
    let with_nul = |mut bytes: Vec<u8>| {
        bytes.push(0);
        bytes
    };

    for (base, head, server) in [
        (
            with_nul(MERGE_BASE.to_vec()),
            text_head.clone(),
            text_server.clone(),
        ),
        (
            MERGE_BASE.to_vec(),
            with_nul(text_head.clone()),
            text_server.clone(),
        ),
        (
            MERGE_BASE.to_vec(),
            text_head.clone(),
            with_nul(text_server.clone()),
        ),
    ] {
        assert_conflict(
            merge::decide(Some(&base), &head, Some(&server)).unwrap(),
            ConflictReason::BinaryChanged,
        );
    }
}

#[test]
fn merge_decide_refuses_a_nul_byte_beyond_the_first_8000_bytes() {
    let mut late_nul_base = vec![b'x'; 9000];
    late_nul_base.extend_from_slice(b"\n\0\n");
    let mut head = late_nul_base.clone();
    head.extend_from_slice(b"head\n");
    let mut server = late_nul_base.clone();
    server.extend_from_slice(b"server\n");

    assert_conflict(
        merge::decide(Some(&late_nul_base), &head, Some(&server)).unwrap(),
        ConflictReason::BinaryChanged,
    );
}

#[test]
fn merge_rules_04_adjacent_edits_conflict() {
    for (head_line, server_line) in [(2, 2), (2, 3)] {
        let head = merge_lines(&[(head_line, "head edit")]);
        let server = merge_lines(&[(server_line, "server edit")]);

        let marked_text = assert_conflict(
            merge::decide(Some(MERGE_BASE), &head, Some(&server)).unwrap(),
            ConflictReason::TextConflict,
        )
        .expect("a text conflict carries its marked text");
        let marked_text = String::from_utf8(marked_text).expect("ASCII fixture");

        for marker in ["<<<<<<< server", "||||||| base", "=======", ">>>>>>> head"] {
            assert!(
                marked_text.contains(marker),
                "missing {marker} for head line {head_line} and server line {server_line}: {marked_text}"
            );
        }
        assert!(marked_text.contains("server edit") && marked_text.contains("head edit"));
    }
}

#[test]
fn merge_decide_keeps_latin_1_bytes_raw_in_marked_text() {
    let base = b"1\n2\n3\n4 caf\xe9\n5\n6\n".to_vec();
    let head = b"1\n2 head\n3\n4 caf\xe9\n5\n6\n".to_vec();
    let server = b"1\n2\n3 server\n4 caf\xe9\n5\n6\n".to_vec();

    let marked_text = assert_conflict(
        merge::decide(Some(&base), &head, Some(&server)).unwrap(),
        ConflictReason::TextConflict,
    )
    .expect("a text conflict carries its marked text");

    assert!(marked_text.contains(&0xe9), "0xE9 must pass through raw");
    assert!(String::from_utf8(marked_text).is_err());
}

#[test]
fn merge_decide_removes_its_workspace_after_every_text_merge() {
    let workspace_parent = TempDir::new().expect("workspace parent should exist");
    let head = merge_lines(&[(2, "line 2 head")]);

    let clean = merge::decide_in(
        workspace_parent.path(),
        Some(MERGE_BASE),
        &head,
        Some(&merge_lines(&[(5, "line 5 server")])),
    )
    .unwrap();
    let conflicting = merge::decide_in(
        workspace_parent.path(),
        Some(MERGE_BASE),
        &head,
        Some(&merge_lines(&[(2, "line 2 server")])),
    )
    .unwrap();

    assert!(matches!(clean, MergeDecision::Merged(_)));
    assert!(matches!(conflicting, MergeDecision::Conflict { .. }));
    assert_eq!(
        fs::read_dir(workspace_parent.path()).unwrap().count(),
        0,
        "every merge must remove its private directory"
    );
}

#[test]
fn merge_decide_reports_a_workspace_failure_as_an_error() {
    let workspace_parent = TempDir::new().expect("workspace parent should exist");
    let missing_parent = workspace_parent.path().join("missing");

    let result = merge::decide_in(
        &missing_parent,
        Some(MERGE_BASE),
        &merge_lines(&[(2, "line 2 head")]),
        Some(&merge_lines(&[(5, "line 5 server")])),
    );

    assert!(matches!(result, Err(BranchDeployError::Other(_))));
}

#[test]
fn git_spawn_error_reports_a_missing_git_as_invalid_arguments() {
    let missing = git_spawn_error(
        std::io::Error::from(std::io::ErrorKind::NotFound),
        "spawning git merge-file",
    );
    let denied = git_spawn_error(
        std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        "spawning git merge-file",
    );

    assert!(
        matches!(missing, BranchDeployError::InvalidArgs(message) if message == "`git` was not found on PATH")
    );
    assert!(
        matches!(denied, BranchDeployError::Other(error) if error.to_string() == "spawning git merge-file")
    );
}

#[test]
fn merge_file_exit_status_maps_to_clean_conflict_or_error() {
    assert_eq!(
        merge::interpret_merge_file_output(Some(0), b"clean".to_vec(), b"").unwrap(),
        MergeDecision::Merged(b"clean".to_vec())
    );
    for conflicts in [1, 2, 127] {
        assert_eq!(
            merge::interpret_merge_file_output(Some(conflicts), b"marked".to_vec(), b"").unwrap(),
            MergeDecision::Conflict {
                reason: ConflictReason::TextConflict,
                marked_text: Some(b"marked".to_vec()),
            }
        );
    }
    for failure in [Some(255), Some(128), Some(-1), None] {
        assert!(
            merge::interpret_merge_file_output(failure, b"partial".to_vec(), b"boom").is_err(),
            "status {failure:?} must be an error"
        );
    }
}

fn plan_for_mode(repository: &TestRepo, base: &str, mode: DeployMode) -> BranchDeployPlan {
    let mut merge_request = request(
        repository.path().to_str().expect("utf-8 path"),
        base,
        "HEAD",
    );
    merge_request.mode = mode;
    plan_branch(&merge_request, &test_profile("/remote/root")).expect("planner should succeed")
}

#[test]
fn merge_planner_records_base_blob_ids_only_in_merge_mode() {
    let repository = TestRepo::new();
    repository.write("changed.txt", b"v1");
    repository.write("restored.txt", b"original");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    repository.write("changed.txt", b"v2");
    repository.write("added.txt", b"new");
    repository.write("restored.txt", b"temporary");
    repository.commit("change");
    repository.write("restored.txt", b"original");
    repository.commit("restore");

    let merge_plan = plan_for_mode(&repository, &base, DeployMode::Merge);

    assert_eq!(
        upload(&merge_plan, "changed.txt").base_object_id,
        Some(repository.rev_parse(&format!("{base}:changed.txt")))
    );
    assert_ne!(
        upload(&merge_plan, "changed.txt").base_object_id.as_deref(),
        Some(upload(&merge_plan, "changed.txt").object_id.as_str())
    );
    assert_eq!(upload(&merge_plan, "added.txt").base_object_id, None);
    let restored = upload(&merge_plan, "restored.txt");
    assert!(
        restored.is_unchanged_in_range(),
        "a path touched but restored to its base blob is unchanged in the range"
    );
    assert!(!upload(&merge_plan, "changed.txt").is_unchanged_in_range());

    let overwrite_plan = plan_for_mode(&repository, &base, DeployMode::Overwrite);
    assert_eq!(overwrite_plan.uploads.len(), 3);
    assert!(overwrite_plan
        .uploads
        .iter()
        .all(|planned| planned.base_object_id.is_none() && !planned.is_unchanged_in_range()));
}

#[cfg(unix)]
#[test]
fn merge_planner_treats_a_symbolic_link_base_as_absent_and_keeps_executable_bases() {
    let repository = TestRepo::new();
    repository.write("target.txt", b"target");
    repository.write("script.sh", b"#!/bin/sh\necho one\n");
    repository.make_executable("script.sh");
    std::os::unix::fs::symlink("target.txt", repository.path().join("was-link.txt"))
        .expect("symlink fixture should exist");
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    fs::remove_file(repository.path().join("was-link.txt")).expect("link should be removed");
    repository.write("was-link.txt", b"now a regular file");
    repository.write("script.sh", b"#!/bin/sh\necho two\n");
    repository.commit("head");

    let plan = plan_for_mode(&repository, &base, DeployMode::Merge);

    assert_eq!(upload(&plan, "was-link.txt").base_object_id, None);
    assert_eq!(
        upload(&plan, "script.sh").base_object_id,
        Some(repository.rev_parse(&format!("{base}:script.sh")))
    );
}

struct MergeFile {
    git_path: &'static str,
    base: Option<Vec<u8>>,
    head: Vec<u8>,
    server: Option<Vec<u8>>,
}

fn merge_file(
    git_path: &'static str,
    base: Option<&[u8]>,
    head: &[u8],
    server: Option<&[u8]>,
) -> MergeFile {
    MergeFile {
        git_path,
        base: base.map(<[u8]>::to_vec),
        head: head.to_vec(),
        server: server.map(<[u8]>::to_vec),
    }
}

/// Builds a merge-mode plan, blob source, and remote for the files, which must be in Git-path order.
/// A base equal to the head shares the head's blob ID, as Git does.
fn merge_setup(files: Vec<MergeFile>) -> (BranchDeployPlan, TestBlobs, TestRemote) {
    let mut plan = BranchDeployPlan::empty("staging", "/repo");
    plan.mode = DeployMode::Merge;
    let mut blobs = TestBlobs::default();
    let mut remote = TestRemote::default();
    for file in files {
        let head_id = format!("head:{}", file.git_path);
        let base_id = file.base.as_ref().map(|base| {
            if *base == file.head {
                head_id.clone()
            } else {
                format!("base:{}", file.git_path)
            }
        });
        if let (Some(id), Some(base)) = (&base_id, &file.base) {
            blobs.blobs.insert(id.clone(), base.clone());
        }
        blobs.blobs.insert(head_id.clone(), file.head.clone());
        let remote_path = format!("/remote/{}", file.git_path);
        if let Some(server) = file.server {
            remote.downloads.insert(remote_path.clone(), server);
        }
        plan.uploads.push(PlannedUpload {
            git_path: file.git_path.to_string(),
            remote_path,
            object_id: head_id,
            bytes: file.head.len() as u64,
            base_object_id: base_id,
        });
    }
    plan.touched_paths = plan.uploads.len();
    (plan, blobs, remote)
}

fn run_merge(files: Vec<MergeFile>) -> (super::BranchDeployManifest, TestRemote, TestBlobs) {
    let (plan, mut blobs, mut remote) = merge_setup(files);
    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);
    (manifest, remote, blobs)
}

fn result_for<'a>(
    manifest: &'a super::BranchDeployManifest,
    git_path: &str,
) -> &'a super::UploadResult {
    manifest
        .uploads
        .iter()
        .find(|result| result.git_path == git_path)
        .unwrap_or_else(|| panic!("missing upload result for {git_path}"))
}

fn writes_to_server(remote: &TestRemote) -> Vec<&RemoteCall> {
    remote
        .calls
        .iter()
        .filter(|call| {
            matches!(
                call,
                RemoteCall::Mkdir(_)
                    | RemoteCall::Upload(_, _)
                    | RemoteCall::Compare(_, _)
                    | RemoteCall::Delete(_)
            )
        })
        .collect()
}

fn downloads_from_server(remote: &TestRemote) -> Vec<&str> {
    remote
        .calls
        .iter()
        .filter_map(|call| match call {
            RemoteCall::Download(path) => Some(path.as_str()),
            _ => None,
        })
        .collect()
}

fn merge_head() -> Vec<u8> {
    merge_lines(&[(2, "line 2 head")])
}

const BINARY_BASE: &[u8] = b"bin\0base";
const BINARY_HEAD: &[u8] = b"bin\0head";

type DecisionRow<'a> = (
    Option<&'a [u8]>,
    Option<&'a [u8]>,
    &'a [u8],
    MergeStatus,
    Option<UploadedFrom>,
);

// (base, server, head, expected reason, whether marked text is produced)
type ConflictRow<'a> = (
    Option<&'a [u8]>,
    Option<Vec<u8>>,
    &'a [u8],
    ConflictReason,
    bool,
);

#[test]
fn merge_rules_01_decision_table() {
    let head = merge_head();
    let server_other_line = merge_lines(&[(5, "line 5 server")]);
    let server_same_line = merge_lines(&[(2, "line 2 server")]);
    let merged = merge_lines(&[(2, "line 2 head"), (5, "line 5 server")]);
    // (base, server, head, expected status, expected upload source)
    let rows: Vec<DecisionRow> = vec![
        (
            Some(BINARY_HEAD),
            Some(b"bin\0other"),
            BINARY_HEAD,
            MergeStatus::UnchangedInRange,
            None,
        ),
        (
            Some(&head),
            None,
            &head,
            MergeStatus::UnchangedInRange,
            None,
        ),
        (
            Some(MERGE_BASE),
            Some(&head),
            &head,
            MergeStatus::AlreadyDeployed,
            None,
        ),
        (None, Some(&head), &head, MergeStatus::AlreadyDeployed, None),
        (
            Some(MERGE_BASE),
            Some(MERGE_BASE),
            &head,
            MergeStatus::FastForward,
            Some(UploadedFrom::HeadBlob),
        ),
        (
            Some(BINARY_BASE),
            Some(BINARY_BASE),
            BINARY_HEAD,
            MergeStatus::FastForward,
            Some(UploadedFrom::HeadBlob),
        ),
        (
            Some(MERGE_BASE),
            Some(&server_other_line),
            &head,
            MergeStatus::Merged,
            Some(UploadedFrom::Merged),
        ),
        (
            Some(MERGE_BASE),
            Some(&server_same_line),
            &head,
            MergeStatus::Conflict,
            None,
        ),
        (
            Some(BINARY_BASE),
            Some(b"bin\0server"),
            BINARY_HEAD,
            MergeStatus::Conflict,
            None,
        ),
        (Some(MERGE_BASE), None, &head, MergeStatus::Conflict, None),
        (
            None,
            None,
            &head,
            MergeStatus::NewFile,
            Some(UploadedFrom::HeadBlob),
        ),
        (
            None,
            Some(&server_other_line),
            &head,
            MergeStatus::Conflict,
            None,
        ),
        // A symbolic-link base reaches the executor as an absent base.
        (
            None,
            None,
            &head,
            MergeStatus::NewFile,
            Some(UploadedFrom::HeadBlob),
        ),
    ];

    for (index, (base, server, row_head, expected_status, expected_source)) in
        rows.into_iter().enumerate()
    {
        let (manifest, remote, _) = run_merge(vec![merge_file("file.txt", base, row_head, server)]);

        let result = result_for(&manifest, "file.txt");
        assert_eq!(
            result.merge_status,
            Some(expected_status),
            "row {}",
            index + 1
        );
        assert_eq!(result.uploaded_from, expected_source, "row {}", index + 1);
        if expected_status == MergeStatus::UnchangedInRange {
            assert!(
                downloads_from_server(&remote).is_empty(),
                "row {}: rule 1 must not download",
                index + 1
            );
        }
        if expected_status == MergeStatus::Merged {
            assert_eq!(result.bytes, merged.len() as u64);
        }
    }
}

#[test]
fn merge_rules_02_conflict_reason_is_reported() {
    let head = merge_head();
    let situations: Vec<ConflictRow> = vec![
        (
            Some(MERGE_BASE),
            Some(merge_lines(&[(2, "line 2 server")])),
            &head,
            ConflictReason::TextConflict,
            true,
        ),
        (
            Some(BINARY_BASE),
            Some(b"bin\0server".to_vec()),
            BINARY_HEAD,
            ConflictReason::BinaryChanged,
            false,
        ),
        (
            Some(MERGE_BASE),
            None,
            &head,
            ConflictReason::DeletedOnServer,
            false,
        ),
        (
            None,
            Some(merge_lines(&[(5, "line 5 server")])),
            &head,
            ConflictReason::AddedOnBoth,
            false,
        ),
    ];

    for (base, server, row_head, expected_reason, has_marked_text) in situations {
        let (manifest, _, _) = run_merge(vec![merge_file(
            "file.txt",
            base,
            row_head,
            server.as_deref(),
        )]);

        let result = result_for(&manifest, "file.txt");
        assert_eq!(result.merge_status, Some(MergeStatus::Conflict));
        assert_eq!(result.conflict_reason, Some(expected_reason));
        assert_eq!(
            result.marked_text.is_some(),
            has_marked_text,
            "{expected_reason:?}"
        );
        assert_eq!(result.marked_text_truncated.is_some(), has_marked_text);
        assert!(
            manifest.failures.iter().any(|failure| {
                failure.stage == "merge" && failure.git_path.as_deref() == Some("file.txt")
            }),
            "{expected_reason:?} must add a merge failure record"
        );
        assert!(!manifest.success);
    }
}

#[test]
fn merge_rules_03_server_only_line_is_preserved() {
    let head = merge_head();
    let server = merge_lines(&[(5, "BCC staging")]);

    let (manifest, remote, _) = run_merge(vec![merge_file(
        "Mailer.php",
        Some(MERGE_BASE),
        &head,
        Some(&server),
    )]);

    let uploaded = remote
        .calls
        .iter()
        .find_map(|call| match call {
            RemoteCall::Upload(_, bytes) => Some(String::from_utf8(bytes.clone()).unwrap()),
            _ => None,
        })
        .expect("the merged file should upload");
    assert!(uploaded.contains("BCC staging") && uploaded.contains("line 2 head"));
    assert!(manifest.success);
}

#[test]
fn merge_rules_05_download_failure_is_not_treated_as_missing() {
    let head = merge_head();
    for server_answer in [
        "451 Local error in processing.",
        "550 Failed to open file, and the parent listing contains the file name.",
    ] {
        let (plan, mut blobs, mut remote) = merge_setup(vec![
            merge_file("a.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
            merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        ]);
        // Call 1 is binary mode and call 2 is the download of a.txt.
        remote
            .failures
            .insert(2, RemoteFailure::operation(server_answer));

        let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

        assert_eq!(
            result_for(&manifest, "a.txt").merge_status,
            Some(MergeStatus::DownloadFailed)
        );
        assert_eq!(
            result_for(&manifest, "b.txt").merge_status,
            Some(MergeStatus::FastForward),
            "the remaining files are still decided"
        );
        assert!(manifest.failures.iter().any(|failure| {
            failure.stage == "download"
                && failure.git_path.as_deref() == Some("a.txt")
                && failure.error == server_answer
        }));
        assert!(manifest.blocked_by_conflicts);
        assert!(writes_to_server(&remote).is_empty());
    }
}

#[test]
fn merge_rules_06_a_550_for_an_absent_file_means_missing() {
    let head = merge_head();

    let (manifest, remote, _) = run_merge(vec![merge_file("added.txt", None, &head, None)]);

    assert_eq!(
        result_for(&manifest, "added.txt").merge_status,
        Some(MergeStatus::NewFile)
    );
    assert!(manifest.success);
    assert!(remote
        .calls
        .contains(&RemoteCall::Upload("/remote/added.txt".to_string(), head)));
}

#[test]
fn merge_blocking_01_one_conflict_blocks_clean_files() {
    let head = merge_head();
    let conflicting_server = merge_lines(&[(2, "line 2 server")]);
    for clean_files in [1usize, 4] {
        let mut files: Vec<MergeFile> = ["a1.txt", "a2.txt", "a3.txt", "a4.txt"]
            .into_iter()
            .take(clean_files)
            .map(|git_path| merge_file(git_path, Some(MERGE_BASE), &head, Some(MERGE_BASE)))
            .collect();
        files.push(merge_file(
            "z-conflict.txt",
            Some(MERGE_BASE),
            &head,
            Some(&conflicting_server),
        ));

        let (manifest, remote, _) = run_merge(files);

        assert!(
            writes_to_server(&remote).is_empty(),
            "{clean_files} clean files"
        );
        assert_eq!(manifest.uploads.len(), clean_files + 1);
        for result in &manifest.uploads {
            assert_eq!(result.upload_status, UploadStatus::NotAttempted);
            assert_eq!(result.verification_status, VerificationStatus::NotAttempted);
        }
        assert_eq!(
            result_for(&manifest, "a1.txt").merge_status,
            Some(MergeStatus::FastForward)
        );
        assert_eq!(
            result_for(&manifest, "z-conflict.txt").merge_status,
            Some(MergeStatus::Conflict)
        );
        assert!(manifest.blocked_by_conflicts);
        assert!(!manifest.success);
        assert_eq!(manifest.counts.uploaded, 0);
    }
}

#[test]
fn merge_blocking_02_connection_lost_while_downloading() {
    let head = merge_head();
    let conflicting_server = merge_lines(&[(2, "line 2 server")]);
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        // c.txt would conflict, so a run that kept deciding after the loss would report it.
        merge_file("c.txt", Some(MERGE_BASE), &head, Some(&conflicting_server)),
    ]);
    // Call 1 is binary mode, call 2 downloads a.txt, and call 3 downloads b.txt.
    remote
        .failures
        .insert(3, RemoteFailure::connection_lost("connection reset"));

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(
        remote.calls,
        vec![
            RemoteCall::Binary,
            RemoteCall::Download("/remote/a.txt".to_string()),
            RemoteCall::Download("/remote/b.txt".to_string()),
        ],
        "no reconnection, no later download, and no upload"
    );
    assert_eq!(
        result_for(&manifest, "a.txt").merge_status,
        Some(MergeStatus::FastForward)
    );
    for git_path in ["b.txt", "c.txt"] {
        let result = result_for(&manifest, git_path);
        assert_eq!(
            result.merge_status,
            Some(MergeStatus::NotDecided),
            "{git_path}"
        );
        assert_eq!(result.upload_status, UploadStatus::NotAttempted);
    }
    assert!(
        manifest
            .failures
            .iter()
            .any(|failure| failure.stage == "download"
                && failure.git_path.as_deref() == Some("b.txt"))
    );
    assert!(manifest.blocked_by_conflicts);
    assert!(!manifest.success);
}

#[test]
fn merge_blocking_03_everything_resolves() {
    let head = merge_head();
    let server_other_line = merge_lines(&[(5, "line 5 server")]);
    let merged = merge_lines(&[(2, "line 2 head"), (5, "line 5 server")]);
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a-unchanged.txt", Some(&head), &head, Some(b"anything")),
        merge_file(
            "b-fast-forward.txt",
            Some(MERGE_BASE),
            &head,
            Some(MERGE_BASE),
        ),
        merge_file(
            "c-merged.txt",
            Some(MERGE_BASE),
            &head,
            Some(&server_other_line),
        ),
        merge_file("d-new.txt", None, &head, None),
        merge_file("e-already.txt", Some(MERGE_BASE), &head, Some(&head)),
    ]);

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    let uploads: Vec<(String, Vec<u8>)> = remote
        .calls
        .iter()
        .filter_map(|call| match call {
            RemoteCall::Upload(path, bytes) => Some((path.clone(), bytes.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        uploads,
        vec![
            ("/remote/b-fast-forward.txt".to_string(), head.clone()),
            ("/remote/c-merged.txt".to_string(), merged),
            ("/remote/d-new.txt".to_string(), head),
        ]
    );
    for git_path in ["a-unchanged.txt", "e-already.txt"] {
        let result = result_for(&manifest, git_path);
        assert_eq!(result.upload_status, UploadStatus::NotNeeded, "{git_path}");
        assert_eq!(result.verification_status, VerificationStatus::NotNeeded);
    }
    for git_path in ["b-fast-forward.txt", "c-merged.txt", "d-new.txt"] {
        let result = result_for(&manifest, git_path);
        assert_eq!(result.upload_status, UploadStatus::Uploaded, "{git_path}");
        assert_eq!(result.verification_status, VerificationStatus::Verified);
    }
    assert_eq!(manifest.counts.uploaded, 3);
    assert_eq!(manifest.counts.verified, 3);
    assert!(!manifest.blocked_by_conflicts);
    assert!(manifest.success);
}

#[test]
fn merge_blocking_04_planning_failure_blocks_merge_uploads() {
    let head = merge_head();
    let (mut plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        merge_file("b.txt", None, &head, None),
    ]);
    plan.failures.push(super::FailureRecord {
        stage: "planning".to_string(),
        git_path: Some("bad\\path".to_string()),
        error: "Git path has an unsafe component".to_string(),
    });

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert!(writes_to_server(&remote).is_empty());
    assert!(manifest.blocked_by_conflicts);
    assert!(!manifest.success);
    assert!(manifest
        .failures
        .iter()
        .any(|failure| failure.stage == "planning"));
    assert!(manifest
        .uploads
        .iter()
        .all(|result| result.upload_status == UploadStatus::NotAttempted));
}

#[test]
fn overwrite_mode_planning_failure_does_not_block_uploads() {
    let mut plan = executor_plan();
    plan.failures.push(super::FailureRecord {
        stage: "planning".to_string(),
        git_path: Some("skipped".to_string()),
        error: "head entry is not a deployable regular blob".to_string(),
    });
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(manifest.counts.uploaded, 2);
    assert!(!manifest.blocked_by_conflicts);
    assert!(!manifest.success);
}

#[test]
fn merge_binary_mode_failure_blocks_before_any_download() {
    let head = merge_head();
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(&head), &head, Some(MERGE_BASE)),
        merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
    ]);
    remote
        .failures
        .insert(1, RemoteFailure::operation("TYPE I refused"));

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(remote.calls, vec![RemoteCall::Binary]);
    assert_eq!(
        result_for(&manifest, "a.txt").merge_status,
        Some(MergeStatus::UnchangedInRange)
    );
    assert_eq!(
        result_for(&manifest, "b.txt").merge_status,
        Some(MergeStatus::NotDecided)
    );
    assert_eq!(
        result_for(&manifest, "b.txt").upload_status,
        UploadStatus::NotAttempted
    );
    assert!(manifest
        .failures
        .iter()
        .any(|failure| failure.stage == "binary_mode"));
    assert!(manifest.blocked_by_conflicts);
    assert!(!manifest.success);
}

#[test]
fn merge_blob_read_failure_blocks_the_run_and_leaves_the_file_undecided() {
    let head = merge_head();
    for missing_blob in ["base:a.txt", "head:a.txt"] {
        let (plan, mut blobs, mut remote) = merge_setup(vec![
            merge_file("a.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
            merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        ]);
        blobs.blobs.remove(missing_blob);

        let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

        assert_eq!(
            result_for(&manifest, "a.txt").merge_status,
            Some(MergeStatus::NotDecided),
            "{missing_blob}"
        );
        assert_eq!(
            result_for(&manifest, "b.txt").merge_status,
            Some(MergeStatus::FastForward)
        );
        assert_eq!(
            downloads_from_server(&remote),
            vec!["/remote/b.txt"],
            "a file whose blob cannot be read is not downloaded"
        );
        assert!(manifest.failures.iter().any(|failure| {
            failure.stage == "read_blob" && failure.git_path.as_deref() == Some("a.txt")
        }));
        assert!(writes_to_server(&remote).is_empty());
        assert!(manifest.blocked_by_conflicts);
    }
}

#[test]
fn upload_to_the_remote_root_creates_no_directory() {
    let mut plan = executor_plan();
    plan.uploads.truncate(1);
    plan.uploads[0].remote_path = "/a.bin".to_string();
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(
        remote.calls,
        vec![
            RemoteCall::Binary,
            RemoteCall::Upload("/a.bin".to_string(), vec![0, b'\r', b'\n', 0xff]),
            RemoteCall::Compare("/a.bin".to_string(), vec![0, b'\r', b'\n', 0xff]),
        ]
    );
    assert!(manifest.success);
}

#[test]
fn merge_blocked_run_without_verification_reports_verification_not_requested() {
    let head = merge_head();
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        merge_file("b.txt", Some(MERGE_BASE), &head, None),
    ]);

    let manifest = execute_deploy(plan, false, &mut blobs, &mut remote);

    let clean = result_for(&manifest, "a.txt");
    assert_eq!(clean.merge_status, Some(MergeStatus::FastForward));
    assert_eq!(clean.upload_status, UploadStatus::NotAttempted);
    assert_eq!(clean.verification_status, VerificationStatus::NotRequested);
    assert!(manifest.blocked_by_conflicts);
}

#[test]
fn overwrite_head_blob_read_failure_skips_only_that_file() {
    let mut blobs = executor_blobs();
    blobs.blobs.remove("a");
    let mut remote = TestRemote::default();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    let failed = result_for(&manifest, "a.bin");
    assert_eq!(failed.upload_status, UploadStatus::Failed);
    assert_eq!(failed.verification_status, VerificationStatus::NotAttempted);
    assert_eq!(
        result_for(&manifest, "nested/b.bin").upload_status,
        UploadStatus::Uploaded
    );
    assert!(manifest.failures.iter().any(|failure| {
        failure.stage == "read_blob" && failure.git_path.as_deref() == Some("a.bin")
    }));
    assert!(!remote
        .calls
        .iter()
        .any(|call| matches!(call, RemoteCall::Upload(path, _) if path == "/remote/a.bin")));
    assert!(!manifest.success);
}

#[test]
fn merge_tool_failure_blocks_the_run_with_a_merge_stage_failure() {
    let head = merge_head();
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file(
            "a.txt",
            Some(MERGE_BASE),
            &head,
            Some(&merge_lines(&[(5, "server")])),
        ),
        merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
    ]);

    let phase = decide_merge_with(&plan, &mut blobs, &mut remote, |base, head, server| {
        if server == Some(MERGE_BASE) {
            merge::decide(base, head, server)
        } else {
            Err(BranchDeployError::Other(anyhow::anyhow!(
                "git merge-file exploded"
            )))
        }
    });

    assert!(phase.is_blocked);
    assert_eq!(phase.outcomes[0].status, MergeStatus::NotDecided);
    assert_eq!(phase.outcomes[1].status, MergeStatus::FastForward);
    assert_eq!(phase.failures.len(), 1);
    assert_eq!(phase.failures[0].stage, "merge");
    assert_eq!(phase.failures[0].git_path.as_deref(), Some("a.txt"));
    assert!(phase.failures[0].error.contains("git merge-file exploded"));
}

#[test]
fn merge_upload_failure_after_a_clean_decision_is_not_a_conflict_block() {
    let head = merge_head();
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(&head), &head, Some(MERGE_BASE)),
        merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        merge_file("c.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
    ]);
    // Calls: binary, download b, download c, mkdir b, upload b.
    remote
        .failures
        .insert(5, RemoteFailure::operation("STOR refused"));

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(
        result_for(&manifest, "b.txt").upload_status,
        UploadStatus::Failed
    );
    assert_eq!(
        result_for(&manifest, "c.txt").upload_status,
        UploadStatus::Uploaded
    );
    assert_eq!(
        result_for(&manifest, "a.txt").upload_status,
        UploadStatus::NotNeeded
    );
    assert!(!manifest.blocked_by_conflicts);
    assert!(!manifest.success);
}

#[test]
fn merge_connection_loss_while_uploading_keeps_not_needed_files_not_needed() {
    let head = merge_head();
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
        merge_file("b.txt", Some(&head), &head, Some(MERGE_BASE)),
        merge_file("c.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
    ]);
    // Calls: binary, download a, download c, mkdir a, upload a fails with a lost connection.
    remote
        .failures
        .insert(5, RemoteFailure::connection_lost("reset"));

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(
        result_for(&manifest, "a.txt").upload_status,
        UploadStatus::Failed
    );
    assert_eq!(
        result_for(&manifest, "b.txt").upload_status,
        UploadStatus::NotNeeded
    );
    assert_eq!(
        result_for(&manifest, "b.txt").verification_status,
        VerificationStatus::NotNeeded
    );
    assert_eq!(
        result_for(&manifest, "c.txt").upload_status,
        UploadStatus::NotAttempted
    );
    assert!(!manifest.blocked_by_conflicts);
}

#[test]
fn merge_verification_disabled_records_not_requested_only_for_uploaded_files() {
    let head = merge_head();
    let (plan, mut blobs, mut remote) = merge_setup(vec![
        merge_file("a.txt", Some(&head), &head, Some(MERGE_BASE)),
        merge_file("b.txt", Some(MERGE_BASE), &head, Some(MERGE_BASE)),
    ]);

    let manifest = execute_deploy(plan, false, &mut blobs, &mut remote);

    assert_eq!(
        result_for(&manifest, "a.txt").verification_status,
        VerificationStatus::NotNeeded
    );
    assert_eq!(
        result_for(&manifest, "b.txt").verification_status,
        VerificationStatus::NotRequested
    );
    assert!(manifest.success);
}

#[test]
fn uploaded_bytes_match() {
    let head = merge_head();
    let merged = merge_lines(&[(2, "line 2 head"), (5, "line 5 server")]);
    let server_other_line = merge_lines(&[(5, "line 5 server")]);

    let (mut overwrite_plan, mut blobs, mut remote) =
        merge_setup(vec![merge_file("f.txt", None, &head, None)]);
    overwrite_plan.mode = DeployMode::Overwrite;
    let overwrite = execute_deploy(overwrite_plan, true, &mut blobs, &mut remote);
    let (merge_head_blob, remote_head, _) = run_merge(vec![merge_file("f.txt", None, &head, None)]);
    let (merge_merged, remote_merged, _) = run_merge(vec![merge_file(
        "f.txt",
        Some(MERGE_BASE),
        &head,
        Some(&server_other_line),
    )]);

    for (manifest, remote, expected_bytes, expected_source) in [
        (overwrite, remote, head.clone(), None),
        (
            merge_head_blob,
            remote_head,
            head.clone(),
            Some(UploadedFrom::HeadBlob),
        ),
        (
            merge_merged,
            remote_merged,
            merged,
            Some(UploadedFrom::Merged),
        ),
    ] {
        let result = result_for(&manifest, "f.txt");
        assert_eq!(result.upload_status, UploadStatus::Uploaded);
        assert_eq!(result.verification_status, VerificationStatus::Verified);
        assert_eq!(result.remote_bytes_read, Some(expected_bytes.len() as u64));
        assert_eq!(result.uploaded_from, expected_source);
        assert!(
            remote.calls.contains(&RemoteCall::Compare(
                "/remote/f.txt".to_string(),
                expected_bytes.clone()
            )),
            "verification must compare with the uploaded bytes"
        );
    }
}

#[test]
fn uploaded_bytes_differ_for_merged_bytes() {
    let head = merge_head();
    let server_other_line = merge_lines(&[(5, "line 5 server")]);
    let (plan, mut blobs, mut remote) = merge_setup(vec![merge_file(
        "f.txt",
        Some(MERGE_BASE),
        &head,
        Some(&server_other_line),
    )]);
    remote.mismatches.insert("/remote/f.txt".to_string());

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert_eq!(
        result_for(&manifest, "f.txt").verification_status,
        VerificationStatus::Mismatch
    );
    assert!(manifest
        .failures
        .iter()
        .any(|failure| failure.stage == "verification"));
    assert!(!manifest.success);
}

#[test]
fn deployment_succeeds() {
    let head = merge_head();
    for mode in [DeployMode::Overwrite, DeployMode::Merge] {
        let (mut plan, mut blobs, mut remote) =
            merge_setup(vec![merge_file("f.txt", None, &head, None)]);
        plan.mode = mode;

        let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

        assert_eq!(manifest.mode, mode);
        assert!(!manifest.blocked_by_conflicts);
        assert!(manifest.success);
        assert_eq!(manifest.counts.uploaded, 1);
        assert_eq!(manifest.counts.verified, 1);
    }
}

#[test]
fn manifest_07_merged_upload_reports_head_blob_and_uploaded_size() {
    let head = merge_head();
    let server = merge_lines(&[(5, "line 5 server with extra length")]);
    let merged = merge_lines(&[(2, "line 2 head"), (5, "line 5 server with extra length")]);

    let (manifest, _, _) = run_merge(vec![merge_file(
        "f.txt",
        Some(MERGE_BASE),
        &head,
        Some(&server),
    )]);

    let result = result_for(&manifest, "f.txt");
    assert_ne!(merged.len(), head.len());
    assert_eq!(result.object_id, "head:f.txt");
    assert_eq!(result.bytes, merged.len() as u64);
    assert_eq!(result.uploaded_from, Some(UploadedFrom::Merged));
}

#[test]
fn manifest_05_long_conflict_text_is_truncated() {
    for (marked_bytes, expected_truncated) in [(200usize, false), (65_536, false), (70_000, true)] {
        let (text, truncated) = merge::marked_text_for_manifest(&vec![b'x'; marked_bytes]);

        assert_eq!(truncated, expected_truncated, "{marked_bytes} bytes");
        assert_eq!(text.len(), marked_bytes.min(65_536));
    }

    // The cut moves back to a character boundary instead of splitting a character.
    let mut straddling = vec![b'x'; 65_535];
    straddling.extend_from_slice("é".as_bytes());
    let (text, truncated) = merge::marked_text_for_manifest(&straddling);
    assert!(truncated);
    assert_eq!(text.len(), 65_535);
}

#[test]
fn manifest_05_long_conflict_text_is_truncated_in_the_manifest() {
    let numbered = |suffix: &str| -> Vec<u8> {
        (0..1000)
            .map(|line| format!("line {line:04} padding padding {suffix}\n"))
            .collect::<String>()
            .into_bytes()
    };
    let (manifest, _, _) = run_merge(vec![merge_file(
        "big.txt",
        Some(&numbered("base")),
        &numbered("head"),
        Some(&numbered("server")),
    )]);

    let result = result_for(&manifest, "big.txt");
    let marked_text = result.marked_text.as_deref().expect("marked text");
    assert_eq!(marked_text.len(), 65_536);
    assert_eq!(result.marked_text_truncated, Some(true));
}

#[test]
fn manifest_06_non_utf8_conflict_text_is_readable() {
    let base = b"1\n2\n3\n4 caf\xe9\n5\n6\n".to_vec();
    let head = b"1\n2 head\n3\n4 caf\xe9\n5\n6\n".to_vec();
    let server = b"1\n2\n3 server\n4 caf\xe9\n5\n6\n".to_vec();

    let (manifest, _, _) = run_merge(vec![merge_file(
        "Mails.php",
        Some(&base),
        &head,
        Some(&server),
    )]);

    let result = result_for(&manifest, "Mails.php");
    let marked_text = result.marked_text.as_deref().expect("marked text");
    assert!(marked_text.contains("caf\u{fffd}"));
    assert!(marked_text.contains("<<<<<<<") && marked_text.contains(">>>>>>>"));
    assert_eq!(result.marked_text_truncated, Some(false));
    let json = serde_json::to_value(result).expect("result serializes");
    assert_eq!(json["marked_text"].as_str(), Some(marked_text));
}

#[test]
fn compatibility_02_branch_deployment_without_mode() {
    let mut remote = TestRemote::default();
    let mut blobs = executor_blobs();

    let manifest = execute_deploy(executor_plan(), true, &mut blobs, &mut remote);

    assert!(manifest.success);
    assert!(
        downloads_from_server(&remote).is_empty(),
        "overwrite mode must not download before it uploads"
    );
    assert!(manifest.uploads.iter().all(|result| {
        result.merge_status.is_none()
            && result.uploaded_from.is_none()
            && result.conflict_reason.is_none()
    }));
}

#[test]
fn head_blobs_05_merge_mode_ignores_working_tree_changes() {
    let repository = TestRepo::new();
    repository.write("tracked.txt", MERGE_BASE);
    repository.commit("base");
    let base = repository.rev_parse("HEAD");
    let head = merge_head();
    repository.write("tracked.txt", &head);
    repository.commit("head");
    repository.write("tracked.txt", b"uncommitted working tree bytes\n");
    let plan = plan_for_mode(&repository, &base, DeployMode::Merge);
    assert!(plan.repository.dirty);
    let mut blobs = BatchBlobReader::new(repository.path()).expect("blob reader should start");
    let mut remote = TestRemote::default();
    remote
        .downloads
        .insert("/remote/root/tracked.txt".to_string(), MERGE_BASE.to_vec());

    let manifest = execute_deploy(plan, true, &mut blobs, &mut remote);

    assert!(manifest.success);
    assert!(manifest.repository.dirty);
    assert_eq!(
        result_for(&manifest, "tracked.txt").merge_status,
        Some(MergeStatus::FastForward)
    );
    assert!(remote.calls.contains(&RemoteCall::Upload(
        "/remote/root/tracked.txt".to_string(),
        head
    )));
}

fn test_profile(remote_root: &str) -> Profile {
    Profile {
        host: "example.test".to_string(),
        port: 21,
        user: "deploy".to_string(),
        remote_root: remote_root.to_string(),
        local_root: ".".to_string(),
        passive: true,
        tls: false,
        accept_invalid_certs: false,
        ignore: Vec::new(),
    }
}

fn request(repo_root: &str, base_ref: &str, head_ref: &str) -> DeployBranchRequest {
    DeployBranchRequest {
        profile: "staging".to_string(),
        repo_root: repo_root.to_string(),
        base_ref: base_ref.to_string(),
        head_ref: head_ref.to_string(),
        verify: true,
        dry_run: true,
        mode: DeployMode::Overwrite,
    }
}

fn deletion_request(
    repository_root: &Path,
    base_commit: &str,
    head_commit: &str,
    paths: Vec<&str>,
    reason: &str,
) -> DeleteBranchFilesRequest {
    DeleteBranchFilesRequest {
        profile: "staging".to_string(),
        repo_root: repository_root.display().to_string(),
        base_commit: base_commit.to_string(),
        head_commit: head_commit.to_string(),
        paths: paths.into_iter().map(str::to_string).collect(),
        reason: reason.to_string(),
        dry_run: false,
    }
}

fn plan_for(repository: &TestRepo, base: &str) -> BranchDeployPlan {
    plan_branch(
        &request(
            repository.path().to_str().expect("utf-8 path"),
            base,
            "HEAD",
        ),
        &test_profile("/remote/root"),
    )
    .expect("planner should succeed")
}

fn assert_invalid(request: &DeployBranchRequest, profile: &Profile) {
    assert!(matches!(
        plan_branch(request, profile),
        Err(BranchDeployError::InvalidArgs(_))
    ));
}

fn upload<'a>(plan: &'a BranchDeployPlan, git_path: &str) -> &'a super::PlannedUpload {
    plan.uploads
        .iter()
        .find(|upload| upload.git_path == git_path)
        .unwrap_or_else(|| panic!("missing planned upload for {git_path}"))
}

fn git_success_at(repository_root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git should run");
    assert!(
        output.status.success(),
        "git command failed: {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_missing_blob(repository_root: &Path, object_id: &str) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(["rev-list", "--objects", "--missing=print", "HEAD"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git should inspect missing objects");
    assert!(
        output.status.success(),
        "git should inspect missing objects: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("?{object_id}")),
        "blob {object_id} must remain absent locally"
    );
}

struct TestRepo {
    directory: TempDir,
}

impl TestRepo {
    fn new() -> Self {
        let directory = TempDir::new().expect("temporary repository should exist");
        let repository = Self { directory };
        repository.git_success(&["init", "--initial-branch=main"]);
        repository.git_success(&["config", "user.name", "Branch Deploy Test"]);
        repository.git_success(&["config", "user.email", "branch-deploy@example.test"]);
        repository
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn write(&self, relative_path: &str, bytes: &[u8]) {
        let path = self.path().join(relative_path);
        let parent = path.parent().expect("fixture file should have a parent");
        fs::create_dir_all(parent).expect("fixture directory should exist");
        fs::write(path, bytes).expect("fixture file should be written");
    }

    #[cfg(unix)]
    fn make_executable(&self, relative_path: &str) {
        let path = self.path().join(relative_path);
        let mut permissions = fs::metadata(&path)
            .expect("fixture metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("fixture should be executable");
    }

    #[cfg(unix)]
    fn add_non_utf8_blob(&self, object_id: &str, relative_path: &[u8]) {
        let mut cache_info = std::ffi::OsString::from(format!("100644,{object_id},"));
        cache_info.push(std::ffi::OsString::from_vec(relative_path.to_vec()));
        let output = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(cache_info)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git should add a non-UTF-8 index entry");
        assert!(
            output.status.success(),
            "git should add a non-UTF-8 index entry: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit(&self, message: &str) {
        self.git_success(&["add", "-A"]);
        self.git_success(&["commit", "-m", message]);
    }

    fn rev_parse(&self, expression: &str) -> String {
        self.git_text(&["rev-parse", expression])
    }

    #[cfg(unix)]
    fn enable_fsmonitor_sentinel(&self) -> std::path::PathBuf {
        let sentinel = self.path().join("fsmonitor-was-invoked");
        let command = self.path().join("fsmonitor-sentinel.sh");
        let script = format!(
            "#!/bin/sh\n: > '{}'\nprintf 'version 2\\n\\n'\n",
            sentinel.display()
        );
        fs::write(&command, script).expect("fsmonitor sentinel should be written");
        let mut permissions = fs::metadata(&command)
            .expect("fsmonitor sentinel metadata should be readable")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&command, permissions)
            .expect("fsmonitor sentinel should be executable");
        self.git_success(&[
            "config",
            "core.fsmonitor",
            command.to_str().expect("utf-8 path"),
        ]);
        sentinel
    }

    fn git_text(&self, arguments: &[&str]) -> String {
        let output = self.git(arguments);
        assert!(output.status.success(), "git command failed: {arguments:?}");
        String::from_utf8(output.stdout)
            .expect("git output should be UTF-8")
            .trim()
            .to_string()
    }

    fn git_success(&self, arguments: &[&str]) {
        let output = self.git(arguments);
        assert!(
            output.status.success(),
            "git command failed: {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new("git");
        command.arg("-C").arg(self.path());
        if arguments.first() == Some(&"commit") {
            command.args(["-c", "core.hooksPath=/dev/null"]);
        }
        command
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Branch Deploy Test")
            .env("GIT_AUTHOR_EMAIL", "branch-deploy@example.test")
            .env("GIT_COMMITTER_NAME", "Branch Deploy Test")
            .env("GIT_COMMITTER_EMAIL", "branch-deploy@example.test")
            .output()
            .expect("git should run")
    }
}
