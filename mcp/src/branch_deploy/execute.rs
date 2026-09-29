use super::merge::{self, MergeDecision};
use super::uniform_results;
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
    let mut failures = plan.failures.clone();
    if let Err(error) = remote.set_binary_mode() {
        failures.push(remote_failure("binary_mode", None, &error));
        let uploads = not_attempted_results(&plan, verify);
        let is_blocked = plan.mode == DeployMode::Merge;
        return manifest_from_execution(plan, verify, uploads, failures, is_blocked);
    }

    let (mut uploads, sources) = match plan.mode {
        DeployMode::Overwrite => (
            uniform_results(
                &plan.uploads,
                UploadStatus::Planned,
                VerificationStatus::planned_for(verify),
            ),
            vec![Some(UploadSource::HeadBlob); plan.uploads.len()],
        ),
        DeployMode::Merge => {
            let phase = decide_merge(&plan, blobs, remote);
            let uploads = merge_results(&plan, &phase, verify);
            failures.extend(phase.failures);
            if phase.is_blocked {
                return manifest_from_execution(plan, verify, uploads, failures, true);
            }
            let sources = phase
                .outcomes
                .into_iter()
                .map(|outcome| outcome.source)
                .collect();
            (uploads, sources)
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

/// Results for a run that transferred nothing because the connection or binary mode failed.
/// In merge mode, files that rule 1 settles are `unchanged_in_range`, and the rest are
/// `not_decided`.
pub(super) fn not_attempted_results(plan: &BranchDeployPlan, verify: bool) -> Vec<UploadResult> {
    match plan.mode {
        DeployMode::Overwrite => uniform_results(
            &plan.uploads,
            UploadStatus::NotAttempted,
            VerificationStatus::not_attempted_for(verify),
        ),
        DeployMode::Merge => plan
            .uploads
            .iter()
            .map(|upload| {
                let outcome = FileOutcome::without_upload(status_before_download(upload));
                merge_result(upload, &outcome, verify, true)
            })
            .collect(),
    }
}

/// Whether the connection can carry the next transfer after one file's upload ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connection {
    Usable,
    Lost,
}

impl Connection {
    fn after(error: &RemoteFailure) -> Self {
        match error.kind {
            RemoteFailureKind::ConnectionLost => Self::Lost,
            RemoteFailureKind::Operation => Self::Usable,
        }
    }
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
    for (index, upload) in plan.uploads.iter().enumerate() {
        let Some(source) = &sources[index] else {
            continue;
        };
        let file = FileUpload {
            upload,
            source,
            result: &mut uploads[index],
        };
        let connection = file.run(verify, blobs, remote, failures);
        if connection == Connection::Lost {
            mark_not_attempted(uploads, index + 1, verify);
            return;
        }
    }
}

/// One planned file in phase 2, with the result it fills in.
struct FileUpload<'a> {
    upload: &'a PlannedUpload,
    source: &'a UploadSource,
    result: &'a mut UploadResult,
}

impl FileUpload<'_> {
    fn run<R: BranchRemote, B: BlobSource>(
        self,
        verify: bool,
        blobs: &mut B,
        remote: &mut R,
        failures: &mut Vec<FailureRecord>,
    ) -> Connection {
        let bytes = match self.source.bytes(self.upload, blobs) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.result.upload_status = UploadStatus::Failed;
                self.result.verification_status = VerificationStatus::NotAttempted;
                failures.push(read_blob_failure(self.upload, &error));
                return Connection::Usable;
            }
        };
        if let Err((stage, error)) = send_file(&self.upload.remote_path, &bytes, remote) {
            self.result.upload_status = UploadStatus::Failed;
            self.result.verification_status = VerificationStatus::NotAttempted;
            failures.push(remote_failure(stage, Some(&self.upload.git_path), &error));
            return Connection::after(&error);
        }
        self.result.upload_status = UploadStatus::Uploaded;
        if !verify {
            return Connection::Usable;
        }
        self.verify(&bytes, remote, failures)
    }

    fn verify<R: BranchRemote>(
        self,
        bytes: &[u8],
        remote: &mut R,
        failures: &mut Vec<FailureRecord>,
    ) -> Connection {
        let comparison = match remote.compare_remote_bytes(&self.upload.remote_path, bytes) {
            Ok(comparison) => comparison,
            Err(error) => {
                self.result.verification_status = VerificationStatus::Failed;
                failures.push(remote_failure(
                    "verification",
                    Some(&self.upload.git_path),
                    &error,
                ));
                return Connection::after(&error);
            }
        };
        self.result.remote_bytes_read = Some(comparison.bytes_read);
        if comparison.matches {
            self.result.verification_status = VerificationStatus::Verified;
        } else {
            self.result.verification_status = VerificationStatus::Mismatch;
            failures.push(FailureRecord {
                stage: "verification".to_string(),
                git_path: Some(self.upload.git_path.clone()),
                error: self.source.mismatch_message().to_string(),
            });
        }
        Connection::Usable
    }
}

/// Creates the parent directory and uploads the bytes. An error names the failed stage.
fn send_file<R: BranchRemote>(
    remote_path: &str,
    bytes: &[u8],
    remote: &mut R,
) -> Result<(), (&'static str, RemoteFailure)> {
    if let Some(parent) = parent_directory(remote_path) {
        remote.mkdir_p(parent).map_err(|error| ("mkdir", error))?;
    }
    remote
        .upload_bytes(remote_path, bytes)
        .map_err(|error| ("upload", error))?;
    Ok(())
}

/// What the upload phase sends for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadSource {
    HeadBlob,
    Merged(Vec<u8>),
}

impl UploadSource {
    /// A head blob is read again here rather than held in memory since phase 1.
    fn bytes<B: BlobSource>(
        &self,
        upload: &PlannedUpload,
        blobs: &mut B,
    ) -> Result<Cow<'_, [u8]>, BranchDeployError> {
        match self {
            Self::HeadBlob => Ok(Cow::Owned(blobs.read_blob(&upload.object_id)?)),
            Self::Merged(merged) => Ok(Cow::Borrowed(merged.as_slice())),
        }
    }

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
    let mut outcomes: Vec<FileOutcome> = plan
        .uploads
        .iter()
        .map(|upload| FileOutcome::without_upload(status_before_download(upload)))
        .collect();
    let mut failures = Vec::new();

    for (outcome, upload) in outcomes.iter_mut().zip(&plan.uploads) {
        if outcome.status != MergeStatus::NotDecided {
            continue;
        }
        let file = decide_file(upload, blobs, remote, &mut decide);
        *outcome = file.outcome;
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

/// Rule 1 needs only blob IDs, so it is settled before any download. Every other file waits
/// for phase 1 as `not_decided`.
fn status_before_download(upload: &PlannedUpload) -> MergeStatus {
    if upload.is_unchanged_in_range() {
        MergeStatus::UnchangedInRange
    } else {
        MergeStatus::NotDecided
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

    fn with_conflict(mut self, conflict: ConflictDetail) -> Self {
        self.outcome.conflict = Some(conflict);
        self
    }
}

/// The three versions of one file that the merge decision reads.
struct FileVersions {
    base: Option<Vec<u8>>,
    head: Vec<u8>,
    server: Option<Vec<u8>>,
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
    let versions = match read_versions(upload, blobs, remote) {
        Ok(versions) => versions,
        Err(failure) => return stopped_before_decision(upload, failure),
    };
    let decision = decide(
        versions.base.as_deref(),
        &versions.head,
        versions.server.as_deref(),
    );
    match decision {
        Ok(decision) => outcome_for_decision(upload, decision),
        Err(error) => FileDecision::stopped(
            MergeStatus::NotDecided,
            merge_failure(upload, error.to_string()),
        ),
    }
}

/// Why a file stopped before all three of its versions were read.
enum VersionReadFailure {
    Blob(BranchDeployError),
    Download(RemoteFailure),
}

/// Reads the head and base blobs, then downloads the server copy.
fn read_versions<R: BranchRemote, B: BlobSource>(
    upload: &PlannedUpload,
    blobs: &mut B,
    remote: &mut R,
) -> Result<FileVersions, VersionReadFailure> {
    let (head, base) = read_head_and_base(upload, blobs).map_err(VersionReadFailure::Blob)?;
    let server = remote
        .download_bytes(&upload.remote_path)
        .map_err(VersionReadFailure::Download)?;
    Ok(FileVersions { base, head, server })
}

fn stopped_before_decision(upload: &PlannedUpload, failure: VersionReadFailure) -> FileDecision {
    match failure {
        VersionReadFailure::Blob(error) => {
            FileDecision::stopped(MergeStatus::NotDecided, read_blob_failure(upload, &error))
        }
        VersionReadFailure::Download(error) => download_failure_decision(upload, &error),
    }
}

fn read_head_and_base<B: BlobSource>(
    upload: &PlannedUpload,
    blobs: &mut B,
) -> Result<(Vec<u8>, Option<Vec<u8>>), BranchDeployError> {
    let head = blobs.read_blob(&upload.object_id)?;
    let base = match &upload.base_object_id {
        Some(object_id) => Some(blobs.read_blob(object_id)?),
        None => None,
    };
    Ok((head, base))
}

/// A lost connection leaves the file undecided and ends phase 1. Any other download error
/// makes the file `download_failed`, and phase 1 goes on to the next file.
fn download_failure_decision(upload: &PlannedUpload, error: &RemoteFailure) -> FileDecision {
    let failure = remote_failure("download", Some(&upload.git_path), error);
    match error.kind {
        RemoteFailureKind::ConnectionLost => FileDecision {
            connection_lost: true,
            ..FileDecision::stopped(MergeStatus::NotDecided, failure)
        },
        RemoteFailureKind::Operation => FileDecision::stopped(MergeStatus::DownloadFailed, failure),
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
            let failure = merge_failure(upload, reason.failure_message().to_string());
            return FileDecision::stopped(MergeStatus::Conflict, failure).with_conflict(
                ConflictDetail {
                    reason,
                    marked_text,
                },
            );
        }
    };
    FileDecision {
        outcome,
        failure: None,
        connection_lost: false,
    }
}

/// The manifest results after phase 1, one per planned file.
fn merge_results(plan: &BranchDeployPlan, phase: &MergePhase, verify: bool) -> Vec<UploadResult> {
    plan.uploads
        .iter()
        .zip(&phase.outcomes)
        .map(|(upload, outcome)| merge_result(upload, outcome, verify, phase.is_blocked))
        .collect()
}

/// One merge-mode result, filled from its phase 1 outcome. In a blocked run every file that
/// would upload is reported as not attempted.
fn merge_result(
    upload: &PlannedUpload,
    outcome: &FileOutcome,
    verify: bool,
    is_blocked: bool,
) -> UploadResult {
    let (upload_status, verification_status) = merge_statuses(outcome.status, verify, is_blocked);
    let mut result = UploadResult::new(upload, upload_status, verification_status);
    result.merge_status = Some(outcome.status);
    if let Some(source) = &outcome.source {
        record_upload_source(&mut result, source);
    }
    if let Some(conflict) = &outcome.conflict {
        record_conflict(&mut result, conflict);
    }
    result
}

fn merge_statuses(
    status: MergeStatus,
    verify: bool,
    is_blocked: bool,
) -> (UploadStatus, VerificationStatus) {
    match status {
        MergeStatus::UnchangedInRange | MergeStatus::AlreadyDeployed => {
            (UploadStatus::NotNeeded, VerificationStatus::NotNeeded)
        }
        _ if is_blocked => (
            UploadStatus::NotAttempted,
            VerificationStatus::not_attempted_for(verify),
        ),
        _ => (
            UploadStatus::Planned,
            VerificationStatus::planned_for(verify),
        ),
    }
}

/// A merged file reports the merged byte count. `object_id` stays the head blob.
fn record_upload_source(result: &mut UploadResult, source: &UploadSource) {
    match source {
        UploadSource::HeadBlob => result.uploaded_from = Some(UploadedFrom::HeadBlob),
        UploadSource::Merged(merged) => {
            result.uploaded_from = Some(UploadedFrom::Merged);
            result.bytes = merged.len() as u64;
        }
    }
}

fn record_conflict(result: &mut UploadResult, conflict: &ConflictDetail) {
    result.conflict_reason = Some(conflict.reason);
    if let Some(marked_text) = &conflict.marked_text {
        let (text, is_truncated) = merge::marked_text_for_manifest(marked_text);
        result.marked_text = Some(text);
        result.marked_text_truncated = Some(is_truncated);
    }
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

fn merge_failure(upload: &PlannedUpload, error: String) -> FailureRecord {
    FailureRecord {
        stage: "merge".to_string(),
        git_path: Some(upload.git_path.clone()),
        error,
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
