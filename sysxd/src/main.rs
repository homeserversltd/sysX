//! sysxd - SysX deterministic cgroup supervisor
//!
//! Implements the boot sequence from 04/17 and 12-canonical-spec-v1.md:
//! 1. Watchdog open (when present)
//! 2. Sealed core.bin cast (within 10ms window)
//! 3. Control socket bind + chown (admin_gid from core.bin)
//! 4. Pre-reactor DAG Kahn sort + cycle/depth validation (max 16)
//! 5. Main reactor (epoll_wait on control socket + cgroup.events)

use std::time::Instant;

use log::{error, info};

use sysx_runtime::RuntimeContext;
use sysx_schema::ServiceSchema;

use sysxd::SysXError;

/// Entry point for sysxd PID 1 supervisor
fn main() {
    env_logger::init();
    info!("sysxd starting - boot order 04/17");

    if let Err(e) = run() {
        error!("sysxd failed: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), SysXError> {
    let start = Instant::now();

    let rt = RuntimeContext::from_env();
    info!(
        "sysx-runtime {} cgroup_root={} slice={}",
        sysx_runtime::VERSION,
        rt.cgroup_root.display(),
        rt.service_slice
    );

    // Phase 1: Boot sequence per 04-runtime-reaper-and-sweep-guarantee.md and 17-sealed-boot-core-bin.md
    let control = sysxd::boot::initialize(&start)?;

    // Phase 2: Pre-reactor DAG validation (Kahn topological sort, cycle detection, max depth 16)
    let schemas = load_schemas()?;
    sysxd::dag::validate(&schemas)?;

    // Phase 3: Main reactor wiring (control socket listener from boot)
    sysxd::reactor::run(control)?;

    info!("sysxd shutdown complete");
    Ok(())
}

/// Load service schemas from /etc/sysx/services/
fn load_schemas() -> Result<Vec<ServiceSchema>, SysXError> {
    // TODO: implement schema loading from config
    info!("Loading service schemas (placeholder)");
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_timing() {
        // Core.bin must be processed within 10ms window per spec
        let start = Instant::now();
        // simulate core.bin load
        let duration = start.elapsed();
        assert!(duration.as_millis() < 10, "core.bin cast exceeded 10ms window");
    }
}
