//! Boot sequence for sysxd - implements 04/17 ordering
//!
//! 1. Watchdog open (when present)
//! 2. core.bin sealed cast within 10ms
//! 3. Control socket bind + chown with admin_gid from core.bin

use std::fs;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::Instant;

use log::{info, warn};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{chown, Gid};
use thiserror::Error;

use crate::SysXError;

const WATCHDOG_PATH: &str = "/dev/watchdog";
const CORE_BIN_PATH: &str = "/etc/sysx/core.bin";
const CONTROL_SOCKET_PATH: &str = "/run/sysx/control.sock";

#[derive(Error, Debug)]
pub enum BootError {
    #[error("Watchdog error: {0}")]
    Watchdog(String),
    #[error("Core.bin cast failed: {0}")]
    CoreBin(String),
    #[error("Socket setup failed: {0}")]
    Socket(String),
}

/// Initialize boot sequence per spec order 04/17
pub fn initialize(start: &Instant) -> Result<(), SysXError> {
    info!("Starting boot sequence (watchdog → core.bin → socket)");

    // Step 1: Watchdog (when present) - must be before core.bin per 17 and 04
    if Path::new(WATCHDOG_PATH).exists() {
        match open(WATCHDOG_PATH, OFlag::O_WRONLY, Mode::empty()) {
            Ok(fd) => {
                info!("Watchdog opened successfully (fd={})", fd);
                // keep fd open or write keepalive pattern
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
    let core_data = load_core_bin()?;
    let core_duration = core_start.elapsed();

    if core_duration.as_millis() > 10 {
        return Err(SysXError::Boot(format!(
            "core.bin cast took {}ms (exceeded 10ms window)",
            core_duration.as_millis()
        )));
    }

    info!("core.bin cast completed in {}ms", core_duration.as_millis());

    // Step 3: Control socket bind + chown (admin_gid from core.bin)
    setup_control_socket(&core_data)?;

    info!("Boot sequence complete");
    Ok(())
}

/// Load sealed core.bin artifact (zero-allocation cast required per spec)
fn load_core_bin() -> Result<Vec<u8>, SysXError> {
    if !Path::new(CORE_BIN_PATH).exists() {
        return Err(SysXError::Boot(format!("{} not found", CORE_BIN_PATH)));
    }

    let data = fs::read(CORE_BIN_PATH)
        .map_err(|e| SysXError::Boot(format!("Failed to read core.bin: {}", e)))?;

    // TODO: parse admin_gid, epoll_timeout_ms, etc. from sealed binary format
    info!("Loaded core.bin ({} bytes)", data.len());
    Ok(data)
}

/// Setup control socket with proper permissions
fn setup_control_socket(core_data: &[u8]) -> Result<(), SysXError> {
    // TODO: bind Unix socket at CONTROL_SOCKET_PATH
    // TODO: chown to admin_gid extracted from core.bin
    info!("Control socket setup (placeholder - bind+chown with admin_gid)");
    Ok(())
}
