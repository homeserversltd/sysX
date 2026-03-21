//! Boot sequence for sysxd - implements 04/17 ordering
//!
//! 1. Watchdog open (when present)
//! 2. core.bin sealed cast within 10ms (`17` §4, `12` §2.2)
//! 3. Control socket bind + chown with admin_gid from core.bin

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Instant;

use log::{info, warn};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{chown, Gid};
use sysx_ipc::{decode_core_bin, SysxCoreBin, CORE_BIN_SIZE};

use crate::SysXError;

/// Artifacts produced by [`initialize`]: bound listener + decoded sealed core (`17`, `12` §2.2).
pub struct BootState {
    pub listener: UnixListener,
    pub core: SysxCoreBin,
}

const WATCHDOG_PATH: &str = "/dev/watchdog";
const CORE_BIN_PATH: &str = "/etc/sysx/core.bin";
pub const CONTROL_SOCKET_PATH: &str = "/run/sysx/control.sock";

/// Initialize boot sequence per spec order 04/17. Returns bound control socket + decoded `core.bin`.
pub fn initialize(_start: &Instant) -> Result<BootState, SysXError> {
    info!("Starting boot sequence (watchdog → core.bin → socket)");

    // Step 1: Watchdog (when present) - must be before core.bin per 17 and 04
    if Path::new(WATCHDOG_PATH).exists() {
        match open(WATCHDOG_PATH, OFlag::O_WRONLY, Mode::empty()) {
            Ok(fd) => {
                info!("Watchdog opened successfully (fd={})", fd);
            }
            Err(e) => {
                warn!("Watchdog open failed (non-fatal if not required): {}", e);
            }
        }
    } else {
        info!("No watchdog device present - skipping");
    }

    // Step 2: Sealed core.bin cast - must complete within 10ms window
    let core_start = Instant::now();
    let core = load_core_bin()?;
    let core_duration = core_start.elapsed();

    if core_duration.as_millis() > 10 {
        return Err(SysXError::Boot(format!(
            "core.bin cast took {}ms (exceeded 10ms window)",
            core_duration.as_millis()
        )));
    }

    info!(
        "core.bin cast completed in {}ms (admin_gid={}, max_cgroups={}, epoll_timeout_ms={})",
        core_duration.as_millis(),
        core.admin_gid,
        core.max_cgroups,
        core.epoll_timeout_ms
    );

    // Step 3: Control socket bind + chown (admin_gid from core.bin)
    let listener = setup_control_socket(&core)?;

    info!("Boot sequence complete");
    Ok(BootState { listener, core })
}

/// Load sealed core.bin artifact — exact `CORE_BIN_SIZE` bytes, `17` §4 magic/version.
fn load_core_bin() -> Result<SysxCoreBin, SysXError> {
    if !Path::new(CORE_BIN_PATH).exists() {
        return Err(SysXError::Boot(format!("{} not found", CORE_BIN_PATH)));
    }

    let data = fs::read(CORE_BIN_PATH)
        .map_err(|e| SysXError::Boot(format!("Failed to read core.bin: {}", e)))?;

    if data.len() != CORE_BIN_SIZE {
        return Err(SysXError::Boot(format!(
            "core.bin must be exactly {} bytes ({}), got {}",
            CORE_BIN_SIZE,
            CORE_BIN_PATH,
            data.len()
        )));
    }

    let core = decode_core_bin(&data).map_err(|e| SysXError::Boot(format!("{}", e)))?;
    info!("Loaded core.bin ({} bytes)", data.len());
    Ok(core)
}

/// Bind Unix domain socket for control plane; chown to `admin_gid` when possible.
fn setup_control_socket(core: &SysxCoreBin) -> Result<UnixListener, SysXError> {
    let admin_gid = core.admin_gid;
    info!("Using admin_gid={} from core.bin", admin_gid);

    let run_dir = Path::new("/run/sysx");
    fs::create_dir_all(run_dir).map_err(|e| {
        SysXError::Boot(format!("create_dir {}: {}", run_dir.display(), e))
    })?;

    if Path::new(CONTROL_SOCKET_PATH).exists() {
        fs::remove_file(CONTROL_SOCKET_PATH).map_err(|e| {
            SysXError::Boot(format!("remove stale socket {}: {}", CONTROL_SOCKET_PATH, e))
        })?;
    }

    let listener = UnixListener::bind(CONTROL_SOCKET_PATH).map_err(|e| {
        SysXError::Boot(format!("bind {}: {}", CONTROL_SOCKET_PATH, e))
    })?;

    if let Ok(meta) = fs::metadata(CONTROL_SOCKET_PATH) {
        let mut perms = meta.permissions();
        perms.set_mode(0o660);
        if let Err(e) = fs::set_permissions(CONTROL_SOCKET_PATH, perms) {
            warn!("chmod control socket: {}", e);
        }
    }

    if let Err(e) = chown(
        CONTROL_SOCKET_PATH,
        None,
        Some(Gid::from_raw(admin_gid as libc::gid_t)),
    ) {
        warn!(
            "chown {} to gid {} failed (non-root in dev VM is OK): {}",
            CONTROL_SOCKET_PATH, admin_gid, e
        );
    } else {
        info!("chown control socket to gid {}", admin_gid);
    }

    use std::os::unix::io::AsRawFd;
    info!(
        "Control socket listening at {} (fd={})",
        CONTROL_SOCKET_PATH,
        listener.as_raw_fd()
    );

    Ok(listener)
}
