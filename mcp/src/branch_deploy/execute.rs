use super::merge::{self, MergeDecision};
use super::{
    deletion_manifest, BranchDeleteManifest, BranchDeletePlan, BranchDeployError,
    BranchDeployManifest, BranchDeployPlan, ConflictReason, DeletePathResult, DeletePathStatus,
    DeployMode, FailureRecord, ManifestCounts, MergeStatus, PlannedUpload, UploadResult,
    UploadStatus, UploadedFrom, VerificationStatus,
};
use std::borrow::Cow;

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
    let is_merge = plan.mode == DeployMode::Merge;
    let mut failures = plan.failures.clone();
    let mut uploads = if is_merge {
        undecided_merge_results(&plan, verify)
    } else {
        plan.uploads
            .iter()
            .map(|upload| {
                UploadResult::new(
                    upload,
                    UploadStatus::Planned,
                    VerificationStatus::planned_for(verify),
                )
            })
            .collect()
    };

    if let Err(error) = remote.set_binary_mode() {
        failures.push(remote_failure("binary_mode", None, &error));
        mark_not_attempted(&mut uploads, 0, verify);
        return manifest_from_execution(plan, verify, uploads, failures, is_merge);
    }

    let sources: Vec<Option<UploadSource>> = match plan.mode {
        DeployMode::Overwrite => plan
            .uploads
            .iter()
            .map(|_| Some(UploadSource::HeadBlob))
            .collect(),
        DeployMode::Merge => {
            let phase = decide_merge(&plan, blobs, remote);
            for ((result, outcome), upload) in
                uploads.iter_mut().zip(&phase.outcomes).zip(&plan.uploads)
            {
                apply_outcome(result, outcome, upload, verify, phase.is_blocked);
            }
            failures.extend(phase.failures);
            if phase.is_blocked {
                return manifest_from_execution(plan, verify, uploads, failures, true);
            }
            phase
                .outcomes
                .into_iter()
                .map(|outcome| outcome.source)
                .collect()
        }
    };

    upload_phase(
        &plan,
        verify,
        blobs,
        remote,
        &sources,
        &mut uploads,
        &mut failures,
    );
    manifest_from_execution(plan, verify, uploads, failures, false)
}

/// Phase 2: uploads each file that has a source, and verifies it against the uploaded bytes.
fn upload_phase<R: BranchRemote, B: BlobSource>(
    plan: &BranchDeployPlan,
    verify: bool,
    blobs: &mut B,
    remote: &mut R,
    sources: &[Option<UploadSource>],
    uploads: &mut [UploadResult],
    failures: &mut Vec<FailureRecord>,
) {
    for index in 0..plan.uploads.len() {
        let upload = &plan.uploads[index];
        let Some(source) = &sources[index] else {
            continue;
        };
        let result = &mut uploads[index];
        let bytes: Cow<[u8]> = match source {
            UploadSource::HeadBlob => match blobs.read_blob(&upload.object_id) {
                Ok(bytes) => Cow::Owned(bytes),
                Err(error) => {
                    result.upload_status = UploadStatus::Failed;
                    result.verification_status = VerificationStatus::NotAttempted;
                    failures.push(read_blob_failure(upload, &error));
                    continue;
                }
            },
            UploadSource::Merged(merged) => Cow::Borrowed(merged.as_slice()),
        };

        let parent = parent_directory(&upload.remote_path);
        if let Some(parent) = parent {
            if let Err(error) = remote.mkdir_p(parent) {
                result.upload_status = UploadStatus::Failed;
                result.verification_status = VerificationStatus::NotAttempted;
                failures.push(remote_failure("mkdir", Some(&upload.git_path), &error));
                if error.kind == RemoteFailureKind::ConnectionLost {
                    mark_not_attempted(uploads, index + 1, verify);
                    return;
                }
                continue;
            }
        }

        if let Err(error) = remote.upload_bytes(&upload.remote_path, &bytes) {
            result.upload_status = UploadStatus::Failed;
            result.verification_status = VerificationStatus::NotAttempted;
            failures.push(remote_failure("upload", Some(&upload.git_path), &error));
            if error.kind == RemoteFailureKind::ConnectionLost {
                mark_not_attempted(uploads, index + 1, verify);
                return;
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
                        error: source.mismatch_message().to_string(),
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
                    mark_not_attempted(uploads, index + 1, verify);
                    return;
                }
            }
        }
    }
}

/// What the upload phase sends for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadSource {
    HeadBlob,
    Merged(Vec<u8>),
}

impl UploadSource {
    fn mismatch_message(&self) -> &'static str {
        match self {
            Self::HeadBlob => "remote bytes do not match the committed blob",
            Self::Merged(_) => "remote bytes do not match the merged bytes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictDetail {
    pub reason: ConflictReason,
    pub marked_text: Option<Vec<u8>>,
}

/// The phase 1 result for one planned file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
    pub status: MergeStatus,
    /// The bytes phase 2 uploads. `None` for a file that does not upload.
    pub source: Option<UploadSource>,
    pub conflict: Option<ConflictDetail>,
}

impl FileOutcome {
    fn without_upload(status: MergeStatus) -> Self {
        Self {
            status,
            source: None,
            conflict: None,
        }
    }

    fn uploading(status: MergeStatus, source: UploadSource) -> Self {
        Self {
            status,
            source: Some(source),
            conflict: None,
        }
    }
}

/// Phase 1 of a merge-mode run: one outcome per planned file, in plan order.
#[derive(Debug, Clone)]
pub struct MergePhase {
    pub outcomes: Vec<FileOutcome>,
    pub failures: Vec<FailureRecord>,
    /// True when phase 2 must not run: a conflict, a failed download or blob read, a merge
    /// tool failure, a lost connection, or a planning failure.
    pub is_blocked: bool,
}

pub fn decide_merge<R: BranchRemote, B: BlobSource>(
    plan: &BranchDeployPlan,
    blobs: &mut B,
    remote: &mut R,
) -> MergePhase {
    decide_merge_with(plan, blobs, remote, merge::decide)
}

pub(super) fn decide_merge_with<R: BranchRemote, B: BlobSource>(
    plan: &BranchDeployPlan,
    blobs: &mut B,
    remote: &mut R,
    mut decide: impl FnMut(
        Option<&[u8]>,
        &[u8],
        Option<&[u8]>,
    ) -> Result<MergeDecision, BranchDeployError>,
) -> MergePhase {
    // Rule 1 needs only blob IDs, so it is settled before any download.
    let mut outcomes: Vec<FileOutcome> = plan
        .uploads
        .iter()
        .map(|upload| {
            FileOutcome::without_upload(if upload.is_unchanged_in_range() {
                MergeStatus::UnchangedInRange
            } else {
                MergeStatus::NotDecided
            })
        })
        .collect();
    let mut failures = Vec::new();

    for (index, upload) in plan.uploads.iter().enumerate() {
        if outcomes[index].status != MergeStatus::NotDecided {
            continue;
        }
        let file = decide_file(upload, blobs, remote, &mut decide);
        outcomes[index] = file.outcome;
        failures.extend(file.failure);
        if file.connection_lost {
            break;
        }
    }

    let is_blocked = !failures.is_empty() || !plan.failures.is_empty();
    MergePhase {
        outcomes,
        failures,
        is_blocked,
    }
}

struct FileDecision {
    outcome: FileOutcome,
    failure: Option<FailureRecord>,
    connection_lost: bool,
}

impl FileDecision {
    fn stopped(status: MergeStatus, failure: FailureRecord) -> Self {
        Self {
            outcome: FileOutcome::without_upload(status),
            failure: Some(failure),
            connection_lost: false,
        }
    }
}

fn decide_file<R: BranchRemote, B: BlobSource>(
    upload: &PlannedUpload,
    blobs: &mut B,
    remote: &mut R,
    decide: &mut impl FnMut(
        Option<&[u8]>,
        &[u8],
        Option<&[u8]>,
    ) -> Result<MergeDecision, BranchDeployError>,
) -> FileDecision {
    let head = match blobs.read_blob(&upload.object_id) {
        Ok(bytes) => bytes,
        Err(error) => {
            return FileDecision::stopped(
                MergeStatus::NotDecided,
                read_blob_failure(upload, &error),
            )
        }
    };
    let base = match &upload.base_object_id {
        Some(object_id) => match blobs.read_blob(object_id) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                return FileDecision::stopped(
                    MergeStatus::NotDecided,
                    read_blob_failure(upload, &error),
                )
            }
        },
        None => None,
    };
    let server = match remote.download_bytes(&upload.remote_path) {
        Ok(server) => server,
        Err(error) => {
            let failure = remote_failure("download", Some(&upload.git_path), &error);
            return match error.kind {
                RemoteFailureKind::ConnectionLost => FileDecision {
                    connection_lost: true,
                    ..FileDecision::stopped(MergeStatus::NotDecided, failure)
                },
                RemoteFailureKind::Operation => {
                    FileDecision::stopped(MergeStatus::DownloadFailed, failure)
                }
            };
        }
    };

    match decide(base.as_deref(), &head, server.as_deref()) {
        Ok(decision) => outcome_for_decision(upload, decision),
        Err(error) => FileDecision::stopped(
            MergeStatus::NotDecided,
            FailureRecord {
                stage: "merge".to_string(),
                git_path: Some(upload.git_path.clone()),
                error: error.to_string(),
            },
        ),
    }
}

fn outcome_for_decision(upload: &PlannedUpload, decision: MergeDecision) -> FileDecision {
    let outcome = match decision {
        MergeDecision::AlreadyDeployed => FileOutcome::without_upload(MergeStatus::AlreadyDeployed),
        MergeDecision::FastForward => {
            FileOutcome::uploading(MergeStatus::FastForward, UploadSource::HeadBlob)
        }
        MergeDecision::Merged(bytes) => {
            FileOutcome::uploading(MergeStatus::Merged, UploadSource::Merged(bytes))
        }
        MergeDecision::NewFile => {
            FileOutcome::uploading(MergeStatus::NewFile, UploadSource::HeadBlob)
        }
        MergeDecision::Conflict {
            reason,
            marked_text,
        } => {
            return FileDecision::stopped(
                MergeStatus::Conflict,
                FailureRecord {
                    stage: "merge".to_string(),
                    git_path: Some(upload.git_path.clone()),
                    error: reason.failure_message().to_string(),
                },
            )
            .with_conflict(ConflictDetail {
                reason,
                marked_text,
            });
        }
    };
    FileDecision {
        outcome,
        failure: None,
        connection_lost: false,
    }
}

impl FileDecision {
    fn with_conflict(mut self, conflict: ConflictDetail) -> Self {
        self.outcome.conflict = Some(conflict);
        self
    }
}

/// Fills the merge fields of one result from its phase 1 outcome. In a blocked run every file
/// that would upload is reported as not attempted.
fn apply_outcome(
    result: &mut UploadResult,
    outcome: &FileOutcome,
    upload: &PlannedUpload,
    verify: bool,
    is_blocked: bool,
) {
    result.merge_status = Some(outcome.status);
    match outcome.status {
        MergeStatus::UnchangedInRange | MergeStatus::AlreadyDeployed => {
            result.upload_status = UploadStatus::NotNeeded;
            result.verification_status = VerificationStatus::NotNeeded;
        }
        _ if is_blocked => {
            result.upload_status = UploadStatus::NotAttempted;
            result.verification_status = VerificationStatus::not_attempted_for(verify);
        }
        _ => {
            result.upload_status = UploadStatus::Planned;
            result.verification_status = VerificationStatus::planned_for(verify);
        }
    }

    result.bytes = upload.bytes;
    match &outcome.source {
        Some(UploadSource::HeadBlob) => result.uploaded_from = Some(UploadedFrom::HeadBlob),
        Some(UploadSource::Merged(merged)) => {
            result.uploaded_from = Some(UploadedFrom::Merged);
            result.bytes = merged.len() as u64;
        }
        None => {}
    }

    if let Some(conflict) = &outcome.conflict {
        result.conflict_reason = Some(conflict.reason);
        if let Some(marked_text) = &conflict.marked_text {
            let (text, is_truncated) = merge::marked_text_for_manifest(marked_text);
            result.marked_text = Some(text);
            result.marked_text_truncated = Some(is_truncated);
        }
    }
}

/// Merge-mode results for a run that decided nothing beyond rule 1: a connection or binary-mode
/// failure. Files that rule 1 settles are `unchanged_in_range`, and the rest are `not_decided`.
pub(super) fn undecided_merge_results(plan: &BranchDeployPlan, verify: bool) -> Vec<UploadResult> {
    plan.uploads
        .iter()
        .map(|upload| {
            let status = if upload.is_unchanged_in_range() {
                MergeStatus::UnchangedInRange
            } else {
                MergeStatus::NotDecided
            };
            let mut result = UploadResult::new(
                upload,
                UploadStatus::NotAttempted,
                VerificationStatus::not_attempted_for(verify),
            );
            apply_outcome(
                &mut result,
                &FileOutcome::without_upload(status),
                upload,
                verify,
                true,
            );
            result
        })
        .collect()
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

/// Marks the still-planned results from `start` on as not attempted. A `not_needed` result
/// stays `not_needed`, because it was never going to upload.
fn mark_not_attempted(uploads: &mut [UploadResult], start: usize, verify: bool) {
    for upload in &mut uploads[start..] {
        if upload.upload_status == UploadStatus::Planned {
            upload.upload_status = UploadStatus::NotAttempted;
            upload.verification_status = VerificationStatus::not_attempted_for(verify);
        }
    }
}

fn read_blob_failure(upload: &PlannedUpload, error: &BranchDeployError) -> FailureRecord {
    FailureRecord {
        stage: "read_blob".to_string(),
        git_path: Some(upload.git_path.clone()),
        error: error.to_string(),
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
    blocked_by_conflicts: bool,
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
        blocked_by_conflicts,
        dry_run: false,
        verify,
        counts,
        uploads,
        deleted: plan.deleted,
        failures,
    }
}
