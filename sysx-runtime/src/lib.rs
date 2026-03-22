//! SysX runtime band: cgroup/namespace/capability policy and fork/exec choreography.
//!
//! Surface types and path helpers used by `sysxd` and `sysx-reaper`. Full policy
//! application is expanded per `12-canonical-spec-v1.md` and `13-sysx-rust-infinite-index-layout.md`.

use std::fs;
use std::io::{self, ErrorKind};
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
            // Unified SysX band under cgroup v2 (`12` §9.1): `/sys/fs/cgroup/sysx/<svc>/`.
            service_slice: "sysx".to_string(),
        }
    }
}

impl RuntimeContext {
    /// Build context from `SYSX_CGROUP_ROOT` / `SYSX_SERVICE_SLICE` if set; otherwise defaults.
    pub fn from_env() -> Self {
        let mut s = Self::default();
        if let Ok(p) = std::env::var("SYSX_CGROUP_ROOT") {
            s.cgroup_root = PathBuf::from(p);
        }
        if let Ok(slice) = std::env::var("SYSX_SERVICE_SLICE") {
            s.service_slice = slice;
        }
        s
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

    /// `memory.events` path (cgroup v2) for OOM oracle probes (`Phase 4`).
    pub fn memory_events_path(&self, service_id: &str) -> Result<PathBuf, RuntimeError> {
        Ok(self.service_cgroup_path(service_id)?.join("memory.events"))
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

/// IPC UTF-8 service name: `15` §1 + `12` §2 — `^[a-z0-9_-]{1,32}$`.
pub fn validate_ipc_service_name(id: &str) -> Result<(), RuntimeError> {
    validate_service_id(id)?;
    if id.len() > 32 {
        return Err(RuntimeError::InvalidServiceId(id.to_string()));
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        return Err(RuntimeError::InvalidServiceId(id.to_string()));
    }
    Ok(())
}

/// Read `populated` from cgroup v2 `cgroup.events` (`12` §1 synchronous status evaluation).
/// Returns `None` if the file is missing; `Some(true)` / `Some(false)` when parsed.
pub fn read_sysfs_cgroup_populated(events_path: &Path) -> io::Result<Option<bool>> {
    match fs::read_to_string(events_path) {
        Ok(s) => Ok(parse_cgroup_events_populated(&s)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read `memory.events` and return whether **`oom_kill`** counter is non-zero (best-effort).
pub fn read_sysfs_memory_oom_kill_nonzero(path: &Path) -> io::Result<bool> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(parse_memory_events_oom_kill(&s).unwrap_or(false)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Parse cgroup v2 `memory.events` for **`oom_kill`** field.
pub fn parse_memory_events_oom_kill(content: &str) -> Option<bool> {
    for line in content.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some("oom_kill") {
            return match it.next() {
                Some(n) => n.parse::<u64>().ok().map(|v| v > 0),
                _ => None,
            };
        }
    }
    None
}

/// Parse `cgroup.events` body for the `populated` field (kernel text format).
pub fn parse_cgroup_events_populated(content: &str) -> Option<bool> {
    for line in content.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some("populated") {
            return match it.next() {
                Some("1") => Some(true),
                Some("0") => Some(false),
                _ => None,
            };
        }
    }
    None
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
            service_slice: "sysx".to_string(),
        };
        let p = ctx.service_cgroup_path("web").unwrap();
        assert!(p.to_string_lossy().ends_with("sysx/web"));
    }

    #[test]
    fn parse_cgroup_events_populated_kernel_shape() {
        let s = "populated 1\nfrozen 0\n";
        assert_eq!(parse_cgroup_events_populated(s), Some(true));
        let s2 = "frozen 0\npopulated 0\n";
        assert_eq!(parse_cgroup_events_populated(s2), Some(false));
    }

    #[test]
    fn parse_memory_events_oom_kill_shape() {
        let s = "low 0\nhigh 0\nmax 0\noom 1\noom_kill 1\n";
        assert_eq!(parse_memory_events_oom_kill(s), Some(true));
        assert_eq!(parse_memory_events_oom_kill("oom_kill 0\n"), Some(false));
    }
}
