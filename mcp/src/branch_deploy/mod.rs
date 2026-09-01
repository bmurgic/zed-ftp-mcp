use crate::config::Profile;
use schemars::JsonSchema;
use serde::Serialize;

mod git;

#[derive(Debug, Clone)]
pub struct DeployBranchRequest {
    pub profile: String,
    pub repo_root: String,
    pub base_ref: String,
    pub head_ref: String,
    pub verify: bool,
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
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UploadResult {
    pub git_path: String,
    pub remote_path: String,
    pub object_id: String,
    #[schemars(transform = crate::schema::remove_unsigned_integer_format)]
    pub bytes: u64,
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

#[derive(Debug, Clone)]
pub struct BranchDeployPlan {
    pub profile: String,
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

pub fn deploy_branch(
    request: &DeployBranchRequest,
    profile: &Profile,
) -> Result<BranchDeployManifest, BranchDeployError> {
    let plan = plan_branch(request, profile)?;
    if request.dry_run {
        return Ok(dry_run_manifest(plan, request.verify));
    }

    Err(BranchDeployError::Other(anyhow::anyhow!(
        "branch deployment execution is not available until upload verification is configured"
    )))
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
        dry_run: true,
        verify,
        counts,
        uploads,
        deleted: plan.deleted,
        failures: plan.failures,
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
