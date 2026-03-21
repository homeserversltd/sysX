//! Per-service **Stop** ladder (`12` §4.1) and DFS post-order unlink (`12` §4.1.1, `18`).
//!
//! Does **not** apply to global §9.1 Terminal Broadcast (see `terminal_broadcast.rs`).

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use log::{error, info, warn};

use sysx_runtime::{read_sysfs_cgroup_populated, RuntimeContext};

/// Step 5 wait (`12` §4.1): hardcoded **5000 ms** (do not increase to mask stalls).
const POPULATED_WAIT_TIMEOUT: Duration = Duration::from_millis(5000);
const POLL_SLICE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// No **`/sys/fs/cgroup/.../<svc>/`** — idempotent stop.
    AlreadyGone,
    /// **`populated=0`** and DFS **`rmdir`** walk completed.
    Dead,
    /// Step 5 timeout — **Tombstoned** (`12` §4.1, `16` §4).
    Tombstoned,
}

/// Run §4.1 steps 1–5, then §4.1.1 on success path.
pub fn stop_service(rt: &RuntimeContext, name: &str) -> Result<StopOutcome, io::Error> {
    let svc = rt.service_cgroup_path(name).map_err(|e| {
        io::Error::new(ErrorKind::InvalidInput, e.to_string())
    })?;
    if !svc.is_dir() {
        info!(
            "Stop {:?}: cgroup dir absent — AlreadyGone (12 §4)",
            name
        );
        return Ok(StopOutcome::AlreadyGone);
    }

    let freeze = svc.join("cgroup.freeze");
    let killf = svc.join("cgroup.kill");
    let events_path = rt.cgroup_events_path(name).map_err(|e| {
        io::Error::new(ErrorKind::InvalidInput, e.to_string())
    })?;

    info!("Stop {:?}: §4.1 step 1 write(1 → cgroup.freeze)", name);
    fs::write(&freeze, b"1")?;

    info!(
        "Stop {:?}: §4.1 step 2 umount2(MNT_DETACH) skipped — no per-service proc mount map (v1)",
        name
    );

    info!("Stop {:?}: §4.1 step 3 write(1 → cgroup.kill)", name);
    fs::write(&killf, b"1")?;

    info!("Stop {:?}: §4.1 step 4 write(0 → cgroup.freeze)", name);
    fs::write(&freeze, b"0")?;

    info!(
        "Stop {:?}: §4.1 step 5 wait populated=0 (timeout {:?})",
        name, POPULATED_WAIT_TIMEOUT
    );
    let deadline = Instant::now() + POPULATED_WAIT_TIMEOUT;
    loop {
        match read_sysfs_cgroup_populated(&events_path) {
            Ok(Some(false)) => {
                info!("Stop {:?}: cgroup.events populated=0", name);
                break;
            }
            Ok(Some(true)) => {}
            Ok(None) => warn!(
                "Stop {:?}: cgroup.events missing under {}",
                name,
                svc.display()
            ),
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            tombstone_kmsg(name);
            return Ok(StopOutcome::Tombstoned);
        }
        thread::sleep(POLL_SLICE);
    }

    info!(
        "Stop {:?}: §4.1.1 DFS post-order rmdir from {}",
        name,
        svc.display()
    );
    dfs_post_order_rmdir(&svc)?;
    Ok(StopOutcome::Dead)
}

fn tombstone_kmsg(name: &str) {
    let line = format!(
        "SYSX KERNEL_STATE_CORRUPTION: stop ladder step 5 timeout populated!=0 service={}",
        name
    );
    error!("{} (12 §4.1 → Tombstoned)", line);
    match OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", line);
        }
        Err(e) => error!("cannot write /dev/kmsg: {}", e),
    }
}

/// Directories only — post-order: children first, then `rmdir` (`12` §4.1.1).
fn dfs_post_order_rmdir(path: &Path) -> io::Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dfs_post_order_rmdir(&entry.path())?;
        }
    }
    fs::remove_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sysx_runtime::RuntimeContext;

    #[test]
    fn dfs_removes_nested_dirs() {
        let base = std::env::temp_dir().join(format!("sysx_dfs_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b")).unwrap();
        dfs_post_order_rmdir(&base).unwrap();
        assert!(!base.exists());
    }

    #[test]
    fn already_gone_no_cgroup() {
        let base = std::env::temp_dir().join(format!("sysx_stop_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let rt = RuntimeContext {
            cgroup_root: base.clone(),
            service_slice: "sysx".to_string(),
        };
        assert!(matches!(
            stop_service(&rt, "nosuch").unwrap(),
            StopOutcome::AlreadyGone
        ));
    }
}
