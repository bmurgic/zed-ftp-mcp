use crate::config::Profile;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod execute;
mod git;
#[allow(dead_code)] // The executor starts using the merge engine in task 1.5.
mod merge;

pub use execute::{
    execute_deletion, execute_deploy, BlobSource, BranchRemote, RemoteComparison, RemoteFailure,
    RemoteFailureKind,
};
use git::BatchBlobReader;

/// How a branch deployment treats files that changed on the server.
/// `overwrite` uploads head blobs as they are. `merge` three-way merges each file with its server copy.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    #[default]
    Overwrite,
    Merge,
}

#[derive(Debug, Clone)]
pub struct DeployBranchRequest {
    pub profile: String,
    pub repo_root: String,
    pub base_ref: String,
    pub head_ref: String,
    pub verify: bool,
    pub dry_run: bool,
    pub mode: DeployMode,
}

#[derive(Debug, Clone)]
pub struct DeleteBranchFilesRequest {
    pub profile: String,
    pub repo_root: String,
    pub base_commit: String,
    pub head_commit: String,
    pub paths: Vec<String>,
    pub reason: String,
    pub dry_run: bool,
}

#[derive(thiserror::Error, Debug)]
pub enum BranchDeployError {
    #[error("{0}")]
    InvalidArgs(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UploadStatus {
    Planned,
    Uploaded,
    Failed,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Planned,
    Verified,
    Mismatch,
    NotRequested,
    Failed,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeletedPathStatus {
    RequiresExplicitCall,
    BlockedCaseCollision,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RepositorySummary {
    pub root: String,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RequestedAndResolvedRef {
    pub requested: String,
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResolvedRefs {
    pub base: RequestedAndResolvedRef,
    pub head: RequestedAndResolvedRef,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ManifestCounts {
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub commits: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub touched_paths: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub planned_uploads: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub uploaded: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub verified: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub deleted_reported: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub failures: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlannedUpload {
    pub git_path: String,
    pub remote_path: String,
    pub object_id: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: u64,
    /// The base-commit blob for this path. It is set only in merge mode, and only when the
    /// path is a regular blob at the base commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_object_id: Option<String>,
}

impl PlannedUpload {
    /// Rule 1 of the merge decision table: the range left this path's blob unchanged.
    #[allow(dead_code)] // The merge executor starts using this in task 1.5.
    pub fn is_unchanged_in_range(&self) -> bool {
        self.base_object_id.as_deref() == Some(self.object_id.as_str())
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UploadResult {
    pub git_path: String,
    pub remote_path: String,
    pub object_id: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub remote_bytes_read: Option<u64>,
    pub upload_status: UploadStatus,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeletedPathResult {
    pub git_path: String,
    pub status: DeletedPathStatus,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FailureRecord {
    pub stage: String,
    pub git_path: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeletePathStatus {
    Planned,
    Deleted,
    Failed,
    NotAttempted,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeletePathResult {
    pub git_path: String,
    pub remote_path: String,
    pub status: DeletePathStatus,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BlockedPath {
    pub git_path: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteManifestCounts {
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub planned: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub deleted: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub failed: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub not_attempted: usize,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub blocked: usize,
}

#[derive(Debug, Clone)]
pub struct BranchDeletePlan {
    pub profile: String,
    pub repository_root: String,
    pub base_commit: String,
    pub head_commit: String,
    pub reason: String,
    pub dry_run: bool,
    pub paths: Vec<DeletePathResult>,
    pub blocked: Vec<BlockedPath>,
    pub failures: Vec<FailureRecord>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchDeleteManifest {
    pub success: bool,
    pub profile: String,
    pub repository_root: String,
    pub base_commit: String,
    pub head_commit: String,
    pub reason: String,
    pub dry_run: bool,
    pub counts: DeleteManifestCounts,
    pub paths: Vec<DeletePathResult>,
    pub blocked: Vec<BlockedPath>,
    pub failures: Vec<FailureRecord>,
}

#[derive(Debug, Clone)]
pub struct BranchDeployPlan {
    pub profile: String,
    pub mode: DeployMode,
    pub repository: RepositorySummary,
    pub refs: ResolvedRefs,
    pub commits: Vec<String>,
    pub touched_paths: usize,
    pub uploads: Vec<PlannedUpload>,
    pub deleted: Vec<DeletedPathResult>,
    pub failures: Vec<FailureRecord>,
}

impl BranchDeployPlan {
    #[cfg(test)]
    pub fn empty(profile: &str, root: &str) -> Self {
        Self {
            profile: profile.to_string(),
            mode: DeployMode::Overwrite,
            repository: RepositorySummary {
                root: root.to_string(),
                dirty: false,
            },
            refs: ResolvedRefs {
                base: RequestedAndResolvedRef {
                    requested: "base".to_string(),
                    commit: "base".to_string(),
                },
                head: RequestedAndResolvedRef {
                    requested: "HEAD".to_string(),
                    commit: "head".to_string(),
                },
            },
            commits: Vec::new(),
            touched_paths: 0,
            uploads: Vec::new(),
            deleted: Vec::new(),
            failures: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchDeployManifest {
    pub success: bool,
    pub profile: String,
    pub repository: RepositorySummary,
    pub refs: ResolvedRefs,
    pub merge_rule: String,
    pub mode: DeployMode,
    pub blocked_by_conflicts: bool,
    pub dry_run: bool,
    pub verify: bool,
    pub counts: ManifestCounts,
    pub uploads: Vec<UploadResult>,
    pub deleted: Vec<DeletedPathResult>,
    pub failures: Vec<FailureRecord>,
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
#[error("{0}")]
pub struct PathFailure(String);

pub fn plan_branch(
    request: &DeployBranchRequest,
    profile: &Profile,
) -> Result<BranchDeployPlan, BranchDeployError> {
    git::plan_branch(request, profile)
}

pub fn plan_deletion(
    request: &DeleteBranchFilesRequest,
    profile: &Profile,
) -> Result<BranchDeletePlan, BranchDeployError> {
    git::plan_deletion(request, profile)
}

pub fn deploy_branch(
    request: &DeployBranchRequest,
    profile: &Profile,
) -> Result<BranchDeployManifest, BranchDeployError> {
    deploy_branch_with_connector(request, profile, crate::ftp::FtpClient::connect)
}

fn deploy_branch_with_connector<F>(
    request: &DeployBranchRequest,
    profile: &Profile,
    connect: F,
) -> Result<BranchDeployManifest, BranchDeployError>
where
    F: FnOnce(&str, &Profile) -> anyhow::Result<crate::ftp::FtpClient>,
{
    deploy_branch_with_dependencies(request, profile, BatchBlobReader::new, connect)
}

fn deploy_branch_with_dependencies<B, S, F>(
    request: &DeployBranchRequest,
    profile: &Profile,
    create_blob_source: B,
    connect: F,
) -> Result<BranchDeployManifest, BranchDeployError>
where
    B: FnOnce(&std::path::Path) -> Result<S, BranchDeployError>,
    S: BlobSource,
    F: FnOnce(&str, &Profile) -> anyhow::Result<crate::ftp::FtpClient>,
{
    let plan = plan_branch(request, profile)?;
    if request.dry_run {
        return Ok(dry_run_manifest(plan, request.verify));
    }

    let mut blobs = create_blob_source(std::path::Path::new(&plan.repository.root))?;
    let mut remote = match connect(&request.profile, profile) {
        Ok(remote) => remote,
        Err(error) => return Ok(connection_failure_manifest(plan, request.verify, error)),
    };
    let manifest = execute_deploy(plan, request.verify, &mut blobs, &mut remote);
    remote.quit();
    Ok(manifest)
}

pub fn delete_branch_files(
    request: &DeleteBranchFilesRequest,
    profile: &Profile,
) -> Result<BranchDeleteManifest, BranchDeployError> {
    delete_branch_files_with_connector(request, profile, crate::ftp::FtpClient::connect)
}

fn delete_branch_files_with_connector<F>(
    request: &DeleteBranchFilesRequest,
    profile: &Profile,
    connect: F,
) -> Result<BranchDeleteManifest, BranchDeployError>
where
    F: FnOnce(&str, &Profile) -> anyhow::Result<crate::ftp::FtpClient>,
{
    let plan = plan_deletion(request, profile)?;
    if !plan.blocked.is_empty() || request.dry_run {
        return Ok(deletion_dry_run_manifest(plan));
    }

    let mut remote = match connect(&request.profile, profile) {
        Ok(remote) => remote,
        Err(error) => return Ok(deletion_connection_failure_manifest(plan, error)),
    };
    let manifest = execute_deletion(plan, &mut remote);
    remote.quit();
    Ok(manifest)
}

pub fn dry_run_manifest(plan: BranchDeployPlan, verify: bool) -> BranchDeployManifest {
    let uploads: Vec<UploadResult> = plan
        .uploads
        .into_iter()
        .map(|upload| UploadResult {
            git_path: upload.git_path,
            remote_path: upload.remote_path,
            object_id: upload.object_id,
            bytes: upload.bytes,
            remote_bytes_read: None,
            upload_status: UploadStatus::Planned,
            verification_status: if verify {
                VerificationStatus::Planned
            } else {
                VerificationStatus::NotRequested
            },
        })
        .collect();
    let counts = ManifestCounts {
        commits: plan.commits.len(),
        touched_paths: plan.touched_paths,
        planned_uploads: uploads.len(),
        uploaded: 0,
        verified: 0,
        deleted_reported: plan.deleted.len(),
        failures: plan.failures.len(),
    };

    BranchDeployManifest {
        success: plan.failures.is_empty(),
        profile: plan.profile,
        repository: plan.repository,
        refs: plan.refs,
        merge_rule: "first_parent".to_string(),
        mode: plan.mode,
        blocked_by_conflicts: false,
        dry_run: true,
        verify,
        counts,
        uploads,
        deleted: plan.deleted,
        failures: plan.failures,
    }
}

pub fn deletion_dry_run_manifest(mut plan: BranchDeletePlan) -> BranchDeleteManifest {
    let preflight_rejected = !plan.blocked.is_empty();
    let dry_run = plan.dry_run;
    let paths = if preflight_rejected {
        Vec::new()
    } else {
        std::mem::take(&mut plan.paths)
            .into_iter()
            .map(|path| DeletePathResult {
                status: DeletePathStatus::Planned,
                ..path
            })
            .collect()
    };
    deletion_manifest(plan, paths, preflight_rejected, dry_run)
}

pub(crate) fn deletion_manifest(
    plan: BranchDeletePlan,
    paths: Vec<DeletePathResult>,
    preflight_rejected: bool,
    dry_run: bool,
) -> BranchDeleteManifest {
    let counts = DeleteManifestCounts {
        planned: paths
            .iter()
            .filter(|path| path.status == DeletePathStatus::Planned)
            .count(),
        deleted: paths
            .iter()
            .filter(|path| path.status == DeletePathStatus::Deleted)
            .count(),
        failed: paths
            .iter()
            .filter(|path| path.status == DeletePathStatus::Failed)
            .count(),
        not_attempted: paths
            .iter()
            .filter(|path| path.status == DeletePathStatus::NotAttempted)
            .count(),
        blocked: plan.blocked.len(),
    };
    BranchDeleteManifest {
        success: !preflight_rejected
            && plan.failures.is_empty()
            && counts.failed == 0
            && counts.not_attempted == 0,
        profile: plan.profile,
        repository_root: plan.repository_root,
        base_commit: plan.base_commit,
        head_commit: plan.head_commit,
        reason: plan.reason,
        dry_run,
        counts,
        paths,
        blocked: plan.blocked,
        failures: plan.failures,
    }
}

fn deletion_connection_failure_manifest(
    plan: BranchDeletePlan,
    error: anyhow::Error,
) -> BranchDeleteManifest {
    let mut paths = plan.paths.clone();
    for path in &mut paths {
        path.status = DeletePathStatus::NotAttempted;
    }
    let mut plan = plan;
    plan.failures.push(FailureRecord {
        stage: "connect".to_string(),
        git_path: None,
        error: error.to_string(),
    });
    deletion_manifest(plan, paths, false, false)
}

fn connection_failure_manifest(
    plan: BranchDeployPlan,
    verify: bool,
    error: anyhow::Error,
) -> BranchDeployManifest {
    let uploads = plan
        .uploads
        .iter()
        .map(|upload| UploadResult {
            git_path: upload.git_path.clone(),
            remote_path: upload.remote_path.clone(),
            object_id: upload.object_id.clone(),
            bytes: upload.bytes,
            remote_bytes_read: None,
            upload_status: UploadStatus::NotAttempted,
            verification_status: if verify {
                VerificationStatus::NotAttempted
            } else {
                VerificationStatus::NotRequested
            },
        })
        .collect::<Vec<_>>();
    let mut failures = plan.failures;
    failures.push(FailureRecord {
        stage: "connect".to_string(),
        git_path: None,
        error: error.to_string(),
    });
    BranchDeployManifest {
        success: false,
        profile: plan.profile,
        repository: plan.repository,
        refs: plan.refs,
        merge_rule: "first_parent".to_string(),
        mode: plan.mode,
        blocked_by_conflicts: false,
        dry_run: false,
        verify,
        counts: ManifestCounts {
            commits: plan.commits.len(),
            touched_paths: plan.touched_paths,
            planned_uploads: uploads.len(),
            uploaded: 0,
            verified: 0,
            deleted_reported: plan.deleted.len(),
            failures: failures.len(),
        },
        uploads,
        deleted: plan.deleted,
        failures,
    }
}

pub(crate) fn map_remote_path(
    remote_root: &str,
    git_path: &[u8],
) -> Result<(String, String), PathFailure> {
    let git_path = std::str::from_utf8(git_path)
        .map_err(|_| PathFailure("Git path is not valid UTF-8".to_string()))?;
    validate_relative_path(git_path, "Git path")?;
    let root = normalize_remote_root(remote_root)?;
    let remote_path = if root == "/" {
        format!("/{git_path}")
    } else {
        format!("{root}/{git_path}")
    };

    if remote_path == root || !remote_path.starts_with(&(root.clone() + "/")) && root != "/" {
        return Err(PathFailure(
            "mapped remote path is not below the configured remote root".to_string(),
        ));
    }

    Ok((git_path.to_string(), remote_path))
}

fn normalize_remote_root(remote_root: &str) -> Result<String, PathFailure> {
    let trimmed = remote_root.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return Ok("/".to_string());
    }
    if !trimmed.starts_with('/') {
        return Err(PathFailure(
            "configured remote root must be absolute".to_string(),
        ));
    }

    let components = trimmed.trim_matches('/');
    validate_relative_path(components, "configured remote root")?;
    Ok(format!("/{components}"))
}

fn validate_relative_path(path: &str, label: &str) -> Result<(), PathFailure> {
    if path.is_empty() || path.starts_with('/') {
        return Err(PathFailure(format!(
            "{label} must be a non-empty relative path"
        )));
    }

    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(PathFailure(format!("{label} has an unsafe component")));
        }
        if component.contains('\\') || component.chars().any(char::is_control) {
            return Err(PathFailure(format!("{label} has an unsafe component")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
