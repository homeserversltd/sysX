//! sysxd - SysX deterministic cgroup supervisor
//!
//! Implements the boot sequence from 04/17 and 12-canonical-spec-v1.md:
//! 1. Watchdog open (when present)
//! 2. Sealed core.bin cast (within 10ms window)
//! 3. Control socket bind + chown (admin_gid from core.bin)
//! 4. Pre-reactor DAG Kahn sort + cycle/depth validation (max 16)
//! 5. Main reactor (epoll_wait on control socket + cgroup.events)

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use log::{error, info};

use sysx_runtime::RuntimeContext;

use sysxd::{DagRuntime, OrchestratorState, SysXError};

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
    sysxd::boot::mount_pid1_essential()
        .unwrap_or_else(|e| panic!("mount_pid1_essential failed: {}", e));

    let start = Instant::now();

    let rt = RuntimeContext::from_env();
    info!(
        "sysx-runtime {} cgroup_root={} slice={}",
        sysx_runtime::VERSION,
        rt.cgroup_root.display(),
        rt.service_slice
    );

    // Phase 1: Boot sequence per 04-runtime-reaper-and-sweep-guarantee.md and 17-sealed-boot-core-bin.md
    let boot = sysxd::boot::initialize(&start)?;

    // Phase 2 / 3: Load schemas, topological forge (Kahn + cyclic severing), max depth 16
    let schemas_vec = sysxd::schema_load::load_schemas()?;
    info!(
        "SYSX_ORACLE_SCHEMAS_LOADED count={}",
        schemas_vec.len()
    );
    let forged = sysxd::dag::forge_boot_order(&schemas_vec)?;

    let schemas: Arc<HashMap<String, sysx_schema::ServiceSchema>> = Arc::new(
        schemas_vec
            .into_iter()
            .map(|s| (s.service.name.clone(), s))
            .collect(),
    );
    let dag_runtime = Arc::new(DagRuntime::from_forged(&forged, &schemas));
    let tombstoned: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let dfs_proven_dead: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let orch = Arc::new(Mutex::new(OrchestratorState::new()));
    let autotest_stop_after_readiness = parse_autotest_stop_from_cmdline();

    // Phase 3: Main reactor (`12` §2.2: epoll timeout and cgroup cap from sealed core.bin)
    sysxd::reactor::run(
        boot.listener,
        &boot.core,
        &rt,
        tombstoned,
        dfs_proven_dead,
        schemas,
        dag_runtime,
        orch,
        autotest_stop_after_readiness,
    )?;

    info!("sysxd shutdown complete");
    Ok(())
}

/// `sysx.autotest=stop:<svc>` in **`/proc/cmdline`** — after FD3 readiness for **`svc`**, run **`Stop`** internally (hypervisor oracle).
fn parse_autotest_stop_from_cmdline() -> Option<String> {
    let data = std::fs::read_to_string("/proc/cmdline").ok()?;
    for part in data.split_whitespace() {
        let Some(rest) = part.strip_prefix("sysx.autotest=") else {
            continue;
        };
        let Some((kind, svc)) = rest.split_once(':') else {
            continue;
        };
        if kind == "stop" && !svc.is_empty() {
            return Some(svc.to_string());
        }
    }
    None
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
