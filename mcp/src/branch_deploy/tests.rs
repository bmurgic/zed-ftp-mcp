use super::git::null_device_for_platform;
use super::git::BatchBlobReader;
use super::{
    deploy_branch, deploy_branch_with_handoff, dry_run_manifest, execute_deploy, map_remote_path,
    plan_branch, BlobSource, BranchDeployError, BranchDeployPlan, BranchRemote, DeletedPathStatus,
    DeployBranchRequest, PlannedUpload, RemoteComparison, RemoteFailure, UploadStatus,
    VerificationStatus,
};
use crate::config::Profile;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteCall {
    Binary,
    Mkdir(String),
    Upload(String, Vec<u8>),
    Compare(String, Vec<u8>),
    #[allow(dead_code)]
    Delete(String),
}

#[derive(Default)]
struct TestRemote {
    calls: Vec<RemoteCall>,
    failures: std::collections::BTreeMap<usize, RemoteFailure>,
    mismatches: std::collections::BTreeSet<String>,
    comparison_bytes_read: std::collections::BTreeMap<String, u64>,
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

fn executor_plan() -> BranchDeployPlan {
    let mut plan = BranchDeployPlan::empty("staging", "/repo");
    plan.uploads = vec![
        PlannedUpload {
            git_path: "a.bin".to_string(),
            remote_path: "/remote/a.bin".to_string(),
            object_id: "a".to_string(),
            bytes: 4,
        },
        PlannedUpload {
            git_path: "nested/b.bin".to_string(),
            remote_path: "/remote/nested/b.bin".to_string(),
            object_id: "b".to_string(),
            bytes: 3,
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
            "counts",
            "deleted",
            "dry_run",
            "failures",
            "merge_rule",
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
    let mut effects = DryRunEffects::default();
    let manifest = deploy_branch_with_handoff(&request, &profile, |_, _| {
        effects.record_execution_handoff();
        Err(branch_execution_unavailable())
    })
    .expect("dry run should succeed");

    assert_eq!(effects.blob_reads(), 0);
    assert_eq!(effects.credential_reads(), 0);
    assert_eq!(effects.remote_factory_calls(), 0);
    assert_eq!(effects.remote_method_calls(), 0);
    assert_eq!(
        manifest
            .uploads
            .iter()
            .map(|upload| upload.upload_status)
            .collect::<Vec<_>>(),
        vec![UploadStatus::Planned; manifest.uploads.len()]
    );

    let mut actual_request = request;
    actual_request.dry_run = false;
    let error = deploy_branch_with_handoff(&actual_request, &profile, |_, _| {
        effects.record_execution_handoff();
        Err(branch_execution_unavailable())
    })
    .expect_err("execution should remain unavailable until Slice 2");
    assert!(error.to_string().contains("not available"));
    assert_eq!(effects.blob_reads(), 1);
    assert_eq!(effects.credential_reads(), 1);
    assert_eq!(effects.remote_factory_calls(), 1);
    assert_eq!(effects.remote_method_calls(), 1);

    let public_manifest = deploy_branch(&actual_request, &profile)
        .expect("connection failures should return an unsuccessful execution manifest");
    assert!(!public_manifest.success);
    assert_eq!(public_manifest.failures[0].stage, "connect");
    assert_eq!(
        public_manifest.uploads[0].upload_status,
        UploadStatus::NotAttempted
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

#[derive(Default)]
struct DryRunEffects {
    blob_reads: usize,
    credential_reads: usize,
    remote_factory_calls: usize,
    remote_method_calls: usize,
}

impl DryRunEffects {
    fn blob_reads(&self) -> usize {
        self.blob_reads
    }

    fn credential_reads(&self) -> usize {
        self.credential_reads
    }

    fn remote_factory_calls(&self) -> usize {
        self.remote_factory_calls
    }

    fn remote_method_calls(&self) -> usize {
        self.remote_method_calls
    }

    fn record_execution_handoff(&mut self) {
        self.blob_reads += 1;
        self.credential_reads += 1;
        self.remote_factory_calls += 1;
        self.remote_method_calls += 1;
    }
}

fn branch_execution_unavailable() -> BranchDeployError {
    BranchDeployError::Other(anyhow::anyhow!(
        "branch deployment execution is not available until upload verification is configured"
    ))
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
