//! Keep Plan: persisting runtime outputs (local folder, Hugging Face repo,
//! and opt-in Google Drive) before a runtime stops or on demand.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::plugins::is_repo_id;

/// Default mount point for `colab drivemount`.
pub const DEFAULT_DRIVE_MOUNT_PATH: &str = "/content/drive";
/// Default destination subdirectory inside the mounted Google Drive.
pub const DEFAULT_DRIVE_DEST_SUBPATH: &str = "MyDrive/colab-playground";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepTargetId {
    Local,
    HuggingFace,
    Drive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalKeepTarget {
    /// Local directory on this computer where archived outputs are saved.
    pub folder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfKeepTarget {
    /// Target Hugging Face repository (`owner/name`).
    pub repo: String,
    /// Optional subdirectory inside the repository (e.g. `checkpoints`).
    #[serde(default)]
    pub path_in_repo: Option<String>,
    /// Optional periodic sync cadence in minutes (`1..=1440`).
    #[serde(default)]
    pub interval_minutes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveKeepTarget {
    /// Absolute VM mount path (default `/content/drive`).
    pub mount_path: String,
    /// Destination subdirectory under `mount_path` (default `MyDrive/colab-playground`).
    pub dest_subpath: String,
    /// Explicit per-session user consent (must never be enabled silently by a plugin).
    #[serde(default)]
    pub consented: bool,
    /// Whether `colab drivemount` has mounted Drive on this runtime.
    #[serde(default)]
    pub mounted: bool,
}

impl Default for DriveKeepTarget {
    fn default() -> Self {
        DriveKeepTarget {
            mount_path: DEFAULT_DRIVE_MOUNT_PATH.to_owned(),
            dest_subpath: DEFAULT_DRIVE_DEST_SUBPATH.to_owned(),
            consented: false,
            mounted: false,
        }
    }
}

/// Per-session persistence plan: VM paths and one or more targets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeepPlan {
    /// Absolute VM paths to persist (e.g. `/content/ComfyUI/output`).
    pub paths: Vec<String>,
    pub local: Option<LocalKeepTarget>,
    pub hf: Option<HfKeepTarget>,
    pub drive: Option<DriveKeepTarget>,
}

impl KeepPlan {
    pub fn has_enabled_target(&self) -> bool {
        self.local.is_some()
            || self.hf.is_some()
            || self.drive.as_ref().is_some_and(|d| d.consented)
    }

    pub fn has_vm_side_target(&self) -> bool {
        self.hf.is_some() || self.drive.as_ref().is_some_and(|d| d.consented)
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.has_enabled_target()
    }

    /// Merges additional VM paths (e.g. from a plugin's `[[keep]]`) without duplicating.
    pub fn merge_paths(&mut self, extra: &[String]) {
        for path in extra {
            let trimmed = path.trim();
            if !trimmed.is_empty() && !self.paths.iter().any(|p| p == trimmed) {
                self.paths.push(trimmed.to_owned());
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        for path in &self.paths {
            validate_vm_path("keep_paths", path)?;
        }
        if let Some(local) = &self.local
            && local.folder.trim().is_empty()
        {
            return Err(Error::invalid("local_folder", "folder path may not be empty"));
        }
        if let Some(hf) = &self.hf {
            if !is_repo_id(hf.repo.trim()) {
                return Err(
                    Error::invalid("hf_repo", "expected owner/name").with("value", &hf.repo)
                );
            }
            if let Some(sub) = &hf.path_in_repo {
                let sub = sub.trim();
                if sub.starts_with('/')
                    || sub.ends_with('/')
                    || sub.split('/').any(|s| s == ".." || s.is_empty())
                    || sub.chars().any(|c| c.is_control())
                {
                    return Err(Error::invalid(
                        "hf_path_in_repo",
                        "must be a relative path without `..`",
                    ));
                }
            }
            if let Some(mins) = hf.interval_minutes
                && !(1..=1440).contains(&mins)
            {
                return Err(Error::invalid(
                    "hf_interval_minutes",
                    "must be between 1 and 1440 minutes",
                ));
            }
        }
        if let Some(drive) = &self.drive {
            validate_vm_path("drive_mount_path", &drive.mount_path)?;
            let sub = drive.dest_subpath.trim();
            if sub.is_empty()
                || sub.starts_with('/')
                || sub.split('/').any(|s| s == ".." || s.is_empty())
                || sub.chars().any(|c| c.is_control())
            {
                return Err(Error::invalid(
                    "drive_dest_subpath",
                    "must be a relative path inside Google Drive without `..`",
                ));
            }
        }
        Ok(())
    }
}

pub fn validate_vm_path(field: &str, path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path.len() > 256
        || path.split('/').any(|c| c == "..")
        || path.chars().any(|c| c.is_control())
    {
        return Err(Error::invalid(
            field,
            "must be an absolute VM path without `..`",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeepTargetResult {
    pub files: u64,
    pub bytes: u64,
    pub destination: String,
    pub error: Option<Error>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeepSyncReport {
    #[serde(with = "crate::clock::serde_secs")]
    pub at: SystemTime,
    pub results: Vec<(KeepTargetId, KeepTargetResult)>,
}

impl KeepSyncReport {
    pub fn all_succeeded(&self) -> bool {
        !self.results.is_empty() && self.results.iter().all(|(_, r)| r.error.is_none())
    }

    pub fn first_error(&self) -> Option<&Error> {
        self.results.iter().find_map(|(_, r)| r.error.as_ref())
    }
}

/// Returns `Err` if `report` has any failed target or no targets ran, ensuring a
/// failed or empty sync never reports `Ok`.
pub fn ensure_report_succeeded(report: &KeepSyncReport) -> Result<()> {
    if let Some(err) = report.first_error() {
        return Err(err.clone());
    }
    if report.results.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput)
            .with("field", "keep_plan")
            .detail("no Keep Plan targets ran"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_paths_repo_and_drive_defaults() {
        let mut plan = KeepPlan {
            paths: vec!["/content/out".into()],
            local: Some(LocalKeepTarget {
                folder: "/tmp/save".into(),
            }),
            hf: Some(HfKeepTarget {
                repo: "alice/model-ckpts".into(),
                path_in_repo: Some("run1/step".into()),
                interval_minutes: Some(15),
            }),
            drive: Some(DriveKeepTarget::default()),
        };
        assert!(plan.validate().is_ok());
        assert!(!plan.drive.as_ref().unwrap().consented);
        assert!(!plan.drive.as_ref().unwrap().mounted);

        plan.paths.push("relative/path".into());
        assert!(plan.validate().is_err());
        plan.paths = vec!["/content/../secret".into()];
        assert!(plan.validate().is_err());
    }

    #[test]
    fn empty_or_failed_report_never_passes_ensure_succeeded() {
        let empty = KeepSyncReport {
            at: SystemTime::UNIX_EPOCH,
            results: vec![],
        };
        assert!(!empty.all_succeeded());
        assert!(ensure_report_succeeded(&empty).is_err());

        let partial_fail = KeepSyncReport {
            at: SystemTime::UNIX_EPOCH,
            results: vec![
                (
                    KeepTargetId::Local,
                    KeepTargetResult {
                        files: 2,
                        bytes: 100,
                        destination: "/tmp/a.tar.gz".into(),
                        error: None,
                    },
                ),
                (
                    KeepTargetId::HuggingFace,
                    KeepTargetResult {
                        files: 0,
                        bytes: 0,
                        destination: "alice/repo".into(),
                        error: Some(Error::new(ErrorKind::Other).detail("upload failed")),
                    },
                ),
            ],
        };
        assert!(!partial_fail.all_succeeded());
        assert!(ensure_report_succeeded(&partial_fail).is_err());
    }
}
