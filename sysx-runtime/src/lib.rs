//! SysX runtime band: cgroup/namespace/capability policy and fork/exec choreography.
//!
//! Surface types and path helpers used by `sysxd` and `sysx-reaper`. Full policy
//! application is expanded per `12-canonical-spec-v1.md` and `13-sysx-rust-infinite-index-layout.md`.

use std::path::{Path, PathBuf};

use log::debug;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Crate version (for logging and install/oracle banners).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runtime configuration loaded from environment and root `index.json` paths (future).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeContext {
    /// Kernel cgroup v2 mount root (default `/sys/fs/cgroup`).
    pub cgroup_root: PathBuf,
    /// Slice name under the unified hierarchy for SysX-managed services.
    pub service_slice: String,
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            service_slice: "sysx.service".to_string(),
        }
    }
}

impl RuntimeContext {
    /// Build context from `SYSX_CGROUP_ROOT` if set; otherwise defaults.
    pub fn from_env() -> Self {
        match std::env::var("SYSX_CGROUP_ROOT") {
            Ok(p) => Self {
                cgroup_root: PathBuf::from(p),
                ..Default::default()
            },
            Err(_) => Self::default(),
        }
    }

    /// Absolute path to the cgroup directory for a named service.
    pub fn service_cgroup_path(&self, service_id: &str) -> Result<PathBuf, RuntimeError> {
        validate_service_id(service_id)?;
        Ok(self
            .cgroup_root
            .join(&self.service_slice)
            .join(service_id))
    }

    /// `cgroup.events` path for a service (reaper/runtime coordination).
    pub fn cgroup_events_path(&self, service_id: &str) -> Result<PathBuf, RuntimeError> {
        Ok(self.service_cgroup_path(service_id)?.join("cgroup.events"))
    }
}

#[derive(Error, Debug)]
pub enum RuntimeError {
    #[error("invalid service id {0:?}: empty, contains '/', or '..'")]
    InvalidServiceId(String),
}

/// Reject path traversal and empty IDs before any cgroup filesystem operation.
pub fn validate_service_id(id: &str) -> Result<(), RuntimeError> {
    if id.is_empty() || id.contains('/') || id.contains("..") {
        return Err(RuntimeError::InvalidServiceId(id.to_string()));
    }
    Ok(())
}

/// Log-only probe: cgroup root exists and is a directory (PID1 / tests).
pub fn cgroup_root_looks_valid(root: &Path) -> bool {
    let ok = root.is_dir();
    debug!(
        "cgroup_root_looks_valid: path={} ok={}",
        root.display(),
        ok
    );
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_service_id_rejects_bad() {
        assert!(validate_service_id("").is_err());
        assert!(validate_service_id("a/b").is_err());
        assert!(validate_service_id("..").is_err());
        assert!(validate_service_id("ok").is_ok());
    }

    #[test]
    fn service_cgroup_path_joins() {
        let ctx = RuntimeContext {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            service_slice: "sysx.service".to_string(),
        };
        let p = ctx.service_cgroup_path("web").unwrap();
        assert!(p.to_string_lossy().ends_with("sysx.service/web"));
    }
}
