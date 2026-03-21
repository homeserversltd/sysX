//! Cgroup slot accounting and **`Command::Start`** mkdir path — `12` §2.2 **`max_cgroups`**.
//!
//! Slot count: subdirectories of **`/sys/fs/cgroup/<slice>/`** whose names pass **`validate_service_id`**
//! (same rule as IPC service names). New **`Start`** consumes a slot only when **`mkdir`** succeeds for
//! a **new** service directory (`12` §2.2 accounting).

use std::fs;
use std::io::{self, ErrorKind};
use log::{error, info, warn};

use sysx_runtime::{validate_service_id, RuntimeContext};

use crate::terminal_broadcast::sysx_slice_root;

/// Count existing service cgroup directories (each occupies one slot until DFS unlink removes it, `12` §2.2).
pub fn count_supervised_service_dirs(rt: &RuntimeContext) -> io::Result<u32> {
    let root = sysx_slice_root(rt);
    if !root.is_dir() {
        return Ok(0);
    }
    let mut n: u32 = 0;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if validate_service_id(name_str).is_ok() {
            n = n.saturating_add(1);
        }
    }
    Ok(n)
}

#[derive(Debug)]
pub enum StartCgroupError {
    AtCapacity { used: u32, max: u32 },
    Mkdir(io::Error),
}

/// Create **`/sys/fs/cgroup/<slice>/<svc>/`** if missing; enforce **`max_cgroups`** before **`mkdir`** (`12` §2.2).
pub fn ensure_start_cgroup(
    rt: &RuntimeContext,
    service_name: &str,
    max_cgroups: u32,
) -> Result<(), StartCgroupError> {
    let path = rt
        .service_cgroup_path(service_name)
        .map_err(|_| StartCgroupError::Mkdir(io::Error::new(ErrorKind::InvalidInput, "service id")))?;
    if path.exists() {
        info!(
            "Start {:?}: cgroup already exists {} — no new slot (12 §2.2)",
            service_name,
            path.display()
        );
        return Ok(());
    }
    let used = count_supervised_service_dirs(rt).map_err(StartCgroupError::Mkdir)?;
    if used >= max_cgroups {
        warn!(
            "Start {:?}: at cgroup capacity used={} max={} from core.bin (12 §2.2) — deny, no mkdir",
            service_name, used, max_cgroups
        );
        return Err(StartCgroupError::AtCapacity { used, max: max_cgroups });
    }
    match fs::create_dir(&path) {
        Ok(()) => {
            info!(
                "Start {:?}: mkdir {} — slot consumed (12 §2.2)",
                service_name,
                path.display()
            );
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            info!(
                "Start {:?}: mkdir EEXIST {} — treating as existing (12 §2.2)",
                service_name,
                path.display()
            );
            Ok(())
        }
        Err(e) => {
            error!("Start {:?}: mkdir {}: {}", service_name, path.display(), e);
            Err(StartCgroupError::Mkdir(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_rt(root: PathBuf) -> RuntimeContext {
        RuntimeContext {
            cgroup_root: root,
            service_slice: "sysx".to_string(),
        }
    }

    #[test]
    fn count_only_valid_service_dirs() {
        let base = std::env::temp_dir().join(format!("sysx_cg_count_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let rt = tmp_rt(base.clone());
        let slice = sysx_slice_root(&rt);
        fs::create_dir_all(slice.join("svc_a")).unwrap();
        fs::create_dir_all(slice.join("svc_b")).unwrap();
        assert_eq!(count_supervised_service_dirs(&rt).unwrap(), 2);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn start_respects_max() {
        let base = std::env::temp_dir().join(format!("sysx_cg_max_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let rt = tmp_rt(base.clone());
        let slice = sysx_slice_root(&rt);
        fs::create_dir_all(slice.join("a")).unwrap();
        assert!(matches!(
            ensure_start_cgroup(&rt, "b", 1),
            Err(StartCgroupError::AtCapacity { .. })
        ));
        assert!(ensure_start_cgroup(&rt, "a", 1).is_ok());
        let _ = fs::remove_dir_all(&base);
    }
}
