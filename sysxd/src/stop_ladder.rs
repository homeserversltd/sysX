//! Per-service **Stop** ladder (`12` §4.1) and DFS post-order unlink (`12` §4.1.1, `18`).
//!
//! Does **not** apply to global §9.1 Terminal Broadcast (see `terminal_broadcast.rs`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::fd::AsFd;
use std::convert::TryFrom;
use std::path::Path;
use std::time::{Duration, Instant};

use log::{error, info, warn};
#[cfg(target_os = "linux")]
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use sysx_runtime::{read_sysfs_cgroup_populated, RuntimeContext};

/// Optional file under **`.../<svc>/.sysx_proc_mount`** containing a single line: mount path for
/// child **`/proc`** when namespace map exists (`12` §4.1 step 2). If present, **`umount2(…, MNT_DETACH)`** runs.
pub const PROC_MOUNT_MARKER: &str = ".sysx_proc_mount";

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

    maybe_umount_proc_detach(&svc, name);

    info!("Stop {:?}: §4.1 step 3 write(1 → cgroup.kill)", name);
    fs::write(&killf, b"1")?;

    info!("Stop {:?}: §4.1 step 4 write(0 → cgroup.freeze)", name);
    fs::write(&freeze, b"0")?;

    info!(
        "Stop {:?}: §4.1 step 5 wait populated=0 (timeout {:?})",
        name, POPULATED_WAIT_TIMEOUT
    );
    let deadline = Instant::now() + POPULATED_WAIT_TIMEOUT;
    wait_populated_zero_pollable(&events_path, name, deadline)?;
    match read_sysfs_cgroup_populated(&events_path) {
        Ok(Some(false)) => {
            info!("Stop {:?}: cgroup.events populated=0", name);
        }
        Ok(Some(true)) => {
            tombstone_kmsg(name);
            return Ok(StopOutcome::Tombstoned);
        }
        Ok(None) => {
            warn!(
                "Stop {:?}: cgroup.events missing after wait under {}",
                name,
                svc.display()
            );
            tombstone_kmsg(name);
            return Ok(StopOutcome::Tombstoned);
        }
        Err(e) => return Err(e),
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

#[cfg(target_os = "linux")]
fn maybe_umount_proc_detach(svc: &Path, name: &str) {
    let marker = svc.join(PROC_MOUNT_MARKER);
    if !marker.is_file() {
        info!(
            "Stop {:?}: §4.1 step 2 no {} — skip umount2",
            name,
            PROC_MOUNT_MARKER
        );
        return;
    }
    let raw = match fs::read_to_string(&marker) {
        Ok(s) => s,
        Err(e) => {
            warn!("Stop {:?}: read {}: {}", name, marker.display(), e);
            return;
        }
    };
    let mp = raw.trim();
    if mp.is_empty() {
        warn!("Stop {:?}: empty proc mount path in {}", name, marker.display());
        return;
    }
    match std::ffi::CString::new(mp) {
        Ok(c) => unsafe {
            let r = libc::umount2(c.as_ptr(), libc::MNT_DETACH);
            if r != 0 {
                let err = io::Error::last_os_error();
                warn!(
                    "Stop {:?}: §4.1 step 2 umount2(MNT_DETACH) {}: {}",
                    name, mp, err
                );
            } else {
                info!(
                    "Stop {:?}: §4.1 step 2 umount2(MNT_DETACH) {}",
                    name, mp
                );
            }
        },
        Err(e) => warn!("Stop {:?}: umount path NUL: {}", name, e),
    }
}

#[cfg(not(target_os = "linux"))]
fn maybe_umount_proc_detach(_svc: &Path, name: &str) {
    info!(
        "Stop {:?}: §4.1 step 2 umount2 skipped (non-Linux)",
        name
    );
}

/// Wait until **`populated=0`** or deadline — uses **`poll(2)`** on **`cgroup.events`** when possible (`11` / `12` §4.1).
fn wait_populated_zero_pollable(
    events_path: &Path,
    name: &str,
    deadline: Instant,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let file = match File::open(events_path) {
            Ok(f) => f,
            Err(e) => {
                warn!(
                    "Stop {:?}: open {} for poll: {} — falling back to sysfs re-read only",
                    name,
                    events_path.display(),
                    e
                );
                return wait_populated_zero_sleep(events_path, name, deadline);
            }
        };
        // cgroup.events updates wake **`poll`** as **`POLLPRI`** (see nix `PollFlags` docs).
        let mut fds = [PollFd::new(
            file.as_fd(),
            PollFlags::POLLPRI | PollFlags::POLLIN,
        )];
        while Instant::now() < deadline {
            match read_sysfs_cgroup_populated(events_path) {
                Ok(Some(false)) => return Ok(()),
                Ok(Some(true)) | Ok(None) => {}
                Err(e) => return Err(e),
            }
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                break;
            }
            let slice = remain.min(POLL_SLICE);
            let timeout = PollTimeout::try_from(slice).unwrap_or(PollTimeout::ZERO);
            if let Err(e) = poll(&mut fds, timeout) {
                warn!("Stop {:?}: poll cgroup.events: {}", name, e);
                std::thread::sleep(slice);
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        wait_populated_zero_sleep(events_path, name, deadline)
    }
}

fn wait_populated_zero_sleep(
    events_path: &Path,
    _name: &str,
    deadline: Instant,
) -> io::Result<()> {
    use std::thread;
    loop {
        match read_sysfs_cgroup_populated(events_path) {
            Ok(Some(false)) => return Ok(()),
            Ok(Some(true)) | Ok(None) => {}
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        thread::sleep(POLL_SLICE);
    }
}

/// Directories only — post-order: children first, then `rmdir` (`12` §4.1.1).
/// Used by **`Command::Reset`** on **`Tombstoned`** recovery (`16` §4).
pub(crate) fn dfs_post_order_rmdir(path: &Path) -> io::Result<()> {
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
