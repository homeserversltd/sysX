//! Terminal Broadcast (global shutdown) — `12` §9.1.
//!
//! Shared path for **`Command::Poweroff`** (`0x50`), **`Command::Reboot`** (`0x60`), and **`SIGPWR`**
//! (ACPI / kernel power path). Step 6 uses **`sync(2)`** then **`reboot(2)`** when PID 1 and
//! **`SYSX_SKIP_REBOOT`** is unset; otherwise logs a clear stub (`12` §9.1, `17` kernel-first policy).

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use libc;
use log::{error, info, warn};
use nix::sys::signal::{kill, Signal, SigSet};
use nix::unistd::Pid;

#[cfg(target_os = "linux")]
use nix::sys::signalfd::{SignalFd, SfdFlags};

use sysx_runtime::{read_sysfs_cgroup_populated, validate_service_id, RuntimeContext};

use crate::SysXError;

/// Final syscall for `12` §9.1 step 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalBroadcastKind {
    /// `LINUX_REBOOT_CMD_POWER_OFF` (poweroff / `SIGPWR` / `0x50`).
    Poweroff,
    /// `LINUX_REBOOT_CMD_RESTART` (`0x60`).
    Reboot,
}

/// Unified SysX cgroup band (`12` §9.1): `/sys/fs/cgroup/<slice>/`.
pub fn sysx_slice_root(rt: &RuntimeContext) -> PathBuf {
    rt.cgroup_root.join(&rt.service_slice)
}

/// Block **`SIGPWR`** and expose it via **`signalfd`** for **`epoll`** integration (`12` §9.1).
#[cfg(target_os = "linux")]
pub fn register_sigpwr_signalfd() -> Result<SignalFd, SysXError> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGPWR);
    mask.thread_block().map_err(|e| {
        SysXError::Reactor(format!("block SIGPWR for signalfd (12 §9.1): {}", e))
    })?;
    SignalFd::with_flags(
        &mask,
        SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC,
    )
    .map_err(|e| SysXError::Reactor(format!("signalfd(SIGPWR): {}", e)))
}

fn should_try_reboot() -> bool {
    if std::env::var_os("SYSX_SKIP_REBOOT").is_some() {
        info!("SYSX_SKIP_REBOOT set — skipping reboot(2) after Terminal Broadcast (12 §9.1)");
        return false;
    }
    let pid = nix::unistd::getpid();
    if pid.as_raw() != 1 {
        info!(
            "pid={} (not PID 1) — stub sync/reboot only; no reboot(2) (12 §9.1 dev/test)",
            pid
        );
        return false;
    }
    true
}

fn read_cgroup_pids(path: &Path) -> Vec<i32> {
    match fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .filter_map(|l| l.trim().parse::<i32>().ok())
            .collect(),
        Err(e) if e.kind() == ErrorKind::NotFound => vec![],
        Err(e) => {
            error!("read cgroup.procs {}: {}", path.display(), e);
            vec![]
        }
    }
}

fn phase1_sigterm_running_services(rt: &RuntimeContext, slice: &Path) {
    let entries = match fs::read_dir(slice) {
        Ok(e) => e,
        Err(e) => {
            error!("read_dir {}: {}", slice.display(), e);
            return;
        }
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if validate_service_id(&name).is_err() {
            continue;
        }
        let events_path = match rt.cgroup_events_path(&name) {
            Ok(p) => p,
            Err(_) => continue,
        };
        match read_sysfs_cgroup_populated(&events_path) {
            Ok(Some(true)) => {
                let procs_path = slice.join(&name).join("cgroup.procs");
                let pids = read_cgroup_pids(&procs_path);
                if pids.is_empty() {
                    info!(
                        "§9.1 phase 1: service {} populated=1 but cgroup.procs empty — skip SIGTERM",
                        name
                    );
                    continue;
                }
                let root_pid = *pids.iter().min().expect("nonempty pids");
                info!(
                    "§9.1 phase 1: service {} root_pid={} → SIGTERM (12 §9.1)",
                    name, root_pid
                );
                if let Err(e) = kill(Pid::from_raw(root_pid), Signal::SIGTERM) {
                    error!("kill SIGTERM pid {} (service {}): {}", root_pid, name, e);
                }
            }
            Ok(Some(false)) | Ok(None) => {}
            Err(e) => error!("read cgroup.events {}: {}", events_path.display(), e),
        }
    }
}

fn phase_global_cgroup_kill(slice: &Path) -> io::Result<()> {
    let freeze = slice.join("cgroup.freeze");
    let killf = slice.join("cgroup.kill");
    fs::write(&freeze, b"1")?;
    fs::write(&killf, b"1")?;
    fs::write(&freeze, b"0")?;
    Ok(())
}

/// `12` §9.1 Terminal Broadcast — strict order: grace SIGTERM → 5000 ms → global freeze/kill/unfreeze → sync → reboot.
pub fn run_terminal_broadcast(rt: &RuntimeContext, kind: TerminalBroadcastKind, trigger: &str) {
    info!(
        "Terminal Broadcast (12 §9.1) start trigger={} kind={:?} slice={}",
        trigger,
        kind,
        sysx_slice_root(rt).display()
    );

    let slice = sysx_slice_root(rt);
    if !slice.is_dir() {
        warn!(
            "sysx slice root not a directory {} — phases 1-5 are best-effort",
            slice.display()
        );
    }

    info!(
        "§9.1 phase 1: SIGTERM to root PID of each Running service under {}",
        slice.display()
    );
    phase1_sigterm_running_services(rt, &slice);

    info!("§9.1 phase 2: synchronous grace 5000 ms (hardcoded, 12 §9.1)");
    thread::sleep(Duration::from_millis(5000));

    info!(
        "§9.1 phases 3-5: write(1→cgroup.freeze), write(1→cgroup.kill), write(0→cgroup.freeze) on {}",
        slice.display()
    );
    match phase_global_cgroup_kill(&slice) {
        Ok(()) => info!("§9.1 phases 3-5: global cgroup freeze/kill/unfreeze complete"),
        Err(e) => error!("§9.1 phases 3-5 failed: {}", e),
    }

    info!("§9.1 phase 6: sync() then reboot(2) (kind={:?})", kind);
    unsafe {
        libc::sync();
    }

    if !should_try_reboot() {
        warn!("Terminal Broadcast complete — reboot(2) stubbed (12 §9.1)");
        return;
    }

    let cmd = match kind {
        TerminalBroadcastKind::Poweroff => libc::LINUX_REBOOT_CMD_POWER_OFF,
        TerminalBroadcastKind::Reboot => libc::LINUX_REBOOT_CMD_RESTART,
    };
    let r = unsafe { libc::reboot(cmd) };
    if r != 0 {
        error!(
            "reboot({}) failed: {} (12 §9.1 phase 6)",
            cmd,
            io::Error::last_os_error()
        );
    }
}

/// Drain pending **`SIGPWR`** frames from **`signalfd`**, then run the same halt path as **`0x50`** (`12` §9.1).
#[cfg(target_os = "linux")]
pub fn on_sigpwr_signalfd(sfd: &SignalFd, rt: &RuntimeContext) {
    let mut n = 0usize;
    loop {
        match sfd.read_signal() {
            Ok(Some(_ssi)) => {
                n += 1;
            }
            Ok(None) => break,
            Err(e) => {
                error!("signalfd read (SIGPWR): {}", e);
                break;
            }
        }
    }
    if n == 0 {
        return;
    }
    info!(
        "SIGPWR: drained {} signalfd frame(s) — Terminal Broadcast halt path (12 §9.1)",
        n
    );
    run_terminal_broadcast(rt, TerminalBroadcastKind::Poweroff, "sigpwr");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_cgroup_pids_parses_lines() {
        let tmp = std::env::temp_dir().join("sysx_tb_test_procs");
        fs::write(&tmp, b"12\n34\n").unwrap();
        let p = read_cgroup_pids(&tmp);
        assert_eq!(p, vec![12, 34]);
        let _ = fs::remove_file(&tmp);
    }
}
