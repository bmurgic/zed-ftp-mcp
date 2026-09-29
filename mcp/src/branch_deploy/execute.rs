use super::{
    deletion_manifest, BranchDeleteManifest, BranchDeletePlan, BranchDeployError,
    BranchDeployManifest, BranchDeployPlan, DeletePathResult, DeletePathStatus, FailureRecord,
    ManifestCounts, UploadResult, UploadStatus, VerificationStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFailureKind {
    Operation,
    ConnectionLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFailure {
    pub kind: RemoteFailureKind,
    pub error: String,
}

impl RemoteFailure {
    pub fn operation(error: impl Into<String>) -> Self {
        Self {
            kind: RemoteFailureKind::Operation,
            error: error.into(),
        }
    }

    #[cfg(test)]
    pub fn connection_lost(error: impl Into<String>) -> Self {
        Self {
            kind: RemoteFailureKind::ConnectionLost,
            error: error.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteComparison {
    pub matches: bool,
    pub bytes_read: u64,
}

pub trait BranchRemote {
    fn set_binary_mode(&mut self) -> Result<(), RemoteFailure>;
    fn mkdir_p(&mut self, path: &str) -> Result<(), RemoteFailure>;
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<u64, RemoteFailure>;
    fn compare_remote_bytes(
        &mut self,
        path: &str,
        expected: &[u8],
    ) -> Result<RemoteComparison, RemoteFailure>;
    fn delete_file(&mut self, path: &str) -> Result<(), RemoteFailure>;
    /// Downloads a server file. `Ok(None)` means the file is missing, which only an FTP 550
    /// reply confirmed by the parent listing may report. Every other error is a failure.
    #[allow(dead_code)] // The merge executor starts calling this in task 1.5.
    fn download_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, RemoteFailure>;
}

pub trait BlobSource {
    fn read_blob(&mut self, object_id: &str) -> Result<Vec<u8>, BranchDeployError>;
}

pub fn execute_deploy<R: BranchRemote, B: BlobSource>(
    plan: BranchDeployPlan,
    verify: bool,
    blobs: &mut B,
    remote: &mut R,
) -> BranchDeployManifest {
    let mut failures = plan.failures.clone();
    let mut uploads = plan
        .uploads
        .iter()
        .map(|upload| UploadResult {
            git_path: upload.git_path.clone(),
            remote_path: upload.remote_path.clone(),
            object_id: upload.object_id.clone(),
            bytes: upload.bytes,
            remote_bytes_read: None,
            upload_status: UploadStatus::Planned,
            verification_status: if verify {
                VerificationStatus::Planned
            } else {
                VerificationStatus::NotRequested
            },
        })
        .collect::<Vec<_>>();

    if let Err(error) = remote.set_binary_mode() {
        failures.push(remote_failure("binary_mode", None, &error));
        mark_not_attempted(&mut uploads, 0, verify);
        return manifest_from_execution(plan, verify, uploads, failures);
    }

    let mut connection_lost = false;
    for index in 0..plan.uploads.len() {
        if connection_lost {
            break;
        }
        let upload = &plan.uploads[index];
        let result = &mut uploads[index];
        let bytes = match blobs.read_blob(&upload.object_id) {
            Ok(bytes) => bytes,
            Err(error) => {
                result.upload_status = UploadStatus::Failed;
                result.verification_status = VerificationStatus::NotAttempted;
                failures.push(FailureRecord {
                    stage: "read_blob".to_string(),
                    git_path: Some(upload.git_path.clone()),
                    error: error.to_string(),
                });
                continue;
            }
        };

        let parent = parent_directory(&upload.remote_path);
        if let Some(parent) = parent {
            if let Err(error) = remote.mkdir_p(parent) {
                result.upload_status = UploadStatus::Failed;
                result.verification_status = VerificationStatus::NotAttempted;
                failures.push(remote_failure("mkdir", Some(&upload.git_path), &error));
                if error.kind == RemoteFailureKind::ConnectionLost {
                    connection_lost = true;
                    mark_not_attempted(&mut uploads, index + 1, verify);
                }
                continue;
            }
        }

        if let Err(error) = remote.upload_bytes(&upload.remote_path, &bytes) {
            result.upload_status = UploadStatus::Failed;
            result.verification_status = VerificationStatus::NotAttempted;
            failures.push(remote_failure("upload", Some(&upload.git_path), &error));
            if error.kind == RemoteFailureKind::ConnectionLost {
                connection_lost = true;
                mark_not_attempted(&mut uploads, index + 1, verify);
            }
            continue;
        }
        result.upload_status = UploadStatus::Uploaded;

        if !verify {
            continue;
        }

        match remote.compare_remote_bytes(&upload.remote_path, &bytes) {
            Ok(comparison) => {
                result.remote_bytes_read = Some(comparison.bytes_read);
                if comparison.matches {
                    result.verification_status = VerificationStatus::Verified;
                } else {
                    result.verification_status = VerificationStatus::Mismatch;
                    failures.push(FailureRecord {
                        stage: "verification".to_string(),
                        git_path: Some(upload.git_path.clone()),
                        error: "remote bytes do not match the committed blob".to_string(),
                    });
                }
            }
            Err(error) => {
                result.verification_status = VerificationStatus::Failed;
                failures.push(remote_failure(
                    "verification",
                    Some(&upload.git_path),
                    &error,
                ));
                if error.kind == RemoteFailureKind::ConnectionLost {
                    connection_lost = true;
                    mark_not_attempted(&mut uploads, index + 1, verify);
                }
            }
        }
    }

    manifest_from_execution(plan, verify, uploads, failures)
}

pub fn execute_deletion<R: BranchRemote>(
    plan: BranchDeletePlan,
    remote: &mut R,
) -> BranchDeleteManifest {
    if !plan.blocked.is_empty() || plan.dry_run {
        return super::deletion_dry_run_manifest(plan);
    }

    let mut paths = plan.paths.clone();
    let mut failures = plan.failures.clone();
    if let Err(error) = remote.set_binary_mode() {
        failures.push(remote_failure("binary_mode", None, &error));
        mark_delete_not_attempted(&mut paths, 0);
        let mut plan = plan;
        plan.failures = failures;
        return deletion_manifest(plan, paths, false, false);
    }

    for index in 0..paths.len() {
        match remote.delete_file(&paths[index].remote_path) {
            Ok(()) => paths[index].status = DeletePathStatus::Deleted,
            Err(error) => {
                paths[index].status = DeletePathStatus::Failed;
                failures.push(remote_failure(
                    "delete",
                    Some(&paths[index].git_path),
                    &error,
                ));
                if error.kind == RemoteFailureKind::ConnectionLost {
                    mark_delete_not_attempted(&mut paths, index + 1);
                    break;
                }
            }
        }
    }

    let mut plan = plan;
    plan.failures = failures;
    deletion_manifest(plan, paths, false, false)
}

fn mark_delete_not_attempted(paths: &mut [DeletePathResult], start: usize) {
    for path in &mut paths[start..] {
        path.status = DeletePathStatus::NotAttempted;
    }
}

fn parent_directory(path: &str) -> Option<&str> {
    path.rsplit_once('/')
        .and_then(|(parent, _)| (!parent.is_empty()).then_some(parent))
}

fn mark_not_attempted(uploads: &mut [UploadResult], start: usize, verify: bool) {
    for upload in &mut uploads[start..] {
        upload.upload_status = UploadStatus::NotAttempted;
        upload.verification_status = if verify {
            VerificationStatus::NotAttempted
        } else {
            VerificationStatus::NotRequested
        };
    }
}

fn remote_failure(stage: &str, git_path: Option<&str>, error: &RemoteFailure) -> FailureRecord {
    FailureRecord {
        stage: stage.to_string(),
        git_path: git_path.map(str::to_string),
        error: error.error.clone(),
    }
}

fn manifest_from_execution(
    plan: BranchDeployPlan,
    verify: bool,
    uploads: Vec<UploadResult>,
    failures: Vec<FailureRecord>,
) -> BranchDeployManifest {
    let counts = ManifestCounts {
        commits: plan.commits.len(),
        touched_paths: plan.touched_paths,
        planned_uploads: uploads.len(),
        uploaded: uploads
            .iter()
            .filter(|upload| upload.upload_status == UploadStatus::Uploaded)
            .count(),
        verified: uploads
            .iter()
            .filter(|upload| upload.verification_status == VerificationStatus::Verified)
            .count(),
        deleted_reported: plan.deleted.len(),
        failures: failures.len(),
    };
    BranchDeployManifest {
        success: failures.is_empty(),
        profile: plan.profile,
        repository: plan.repository,
        refs: plan.refs,
        merge_rule: "first_parent".to_string(),
        mode: plan.mode,
        blocked_by_conflicts: false,
        dry_run: false,
        verify,
        counts,
        uploads,
        deleted: plan.deleted,
        failures,
    }
}
