//! SysX Reaper CLI
//!
//! Entry point for the cgroup.events epoll monitor and sweep coordinator.
//! Implements the epoll side of the stop ladder per 12-canonical-spec-v1.md §4.1.1.

use std::env;
use sysx_reaper::Reaper;
use log::{info, error};

fn main() {
    env_logger::init();

    info!("=== SysX Reaper starting ===");
    info!("cgroup.events epoll monitor + sweep coordination hooks");
    info!("Per 04-cgroup-events-epoll-and-sweep-coordination.md");

    let mut reaper = Reaper::new();

    if let Err(e) = reaper.init() {
        error!("Failed to initialize Reaper: {}", e);
        std::process::exit(1);
    }

    // Example registration (in real usage this would come from sysxd core)
    if let Err(e) = reaper.register_cgroup(
        "/sys/fs/cgroup/service/test-service".into(),
        "test-service".to_string()
    ) {
        error!("Failed to register test cgroup: {}", e);
    }

    match reaper.wait_for_sweep_ready(5000) {
        Ok(ready) => {
            info!("Services ready for sweep: {:?}", ready);
            if let Err(e) = reaper.perform_sweep(&ready) {
                error!("Sweep failed: {}", e);
            } else {
                info!("Sweep coordination completed successfully");
            }
        }
        Err(e) => error!("Wait for sweep failed: {}", e),
    }

    info!("SysX Reaper shutdown complete");
}
