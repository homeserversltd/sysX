//! Runtime DAG orchestration — bootstrap order, readiness → Running, cascade failure (`Phase 3`).
//!
//! ## Cascade failure (red-team / blind spot)
//!
//! If service **B** enters **`Failed`** (e.g. readiness **EOF** before FD3 bytes, or upstream **`cascade_failed`**),
//! any service **A** with **B ∈ depends_on** must not stay **`Offline`** forever waiting for **B**.
//!
//! **Mechanism:** **`cascade_failed`** is a closed set under the **dependents** relation (reverse DAG edges).
//! When **B** is marked failed, **`propagate_cascade_failure`** inserts **B** and **BFS** inserts every service
//! that transitively depends on **B**. **`Status`** for those services returns **`Failed` + Orphaned** (`0x05` / `0x01`),
//! and **`try_spawn_ready_services`** skips them. New spawns are denied if any dependency is in **`cascade_failed`**.
//!
//! This **severs** orphaned dependency edges mathematically: **A** is not "pending forever"; it is **`Failed`**
//! with reason **Orphaned** until **`Reset`** / operator intervention.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use log::{error, warn};

use crate::dag::DagRuntime;

/// Epistemic runtime: who is running, who is awaiting FD3, who is failed by cascade.
#[derive(Debug)]
pub struct OrchestratorState {
    pub running: HashSet<String>,
    pub starting: HashSet<String>,
    pub cascade_failed: HashSet<String>,
    /// Immediate child PID returned by **`Command::spawn`** after **`execve`** path is set up (`service_spawn`).
    /// **O(1)** SIGTERM target for **`12` §9.1** Terminal Broadcast — no **`cgroup.procs`** scan during grace.
    pub root_pids: HashMap<String, i32>,
    /// Services in circuit-backoff wait (timerfd armed).
    pub recovering: HashSet<String>,
    /// Service that exceeded **`max_restarts`** in **`window_sec`** — **`Status`** Failed + **`CircuitOpen`** (not Orphaned).
    pub circuit_origin: HashSet<String>,
    /// Monotonic restart failure timestamps for sliding **`window_sec`** (recovery policy).
    pub restart_events: HashMap<String, Vec<Instant>>,
    /// Exponential backoff generation per service (incremented each scheduled recovery; reset on FD3).
    pub recovery_generation: HashMap<String, u32>,
}

impl Default for OrchestratorState {
    fn default() -> Self {
        Self {
            running: HashSet::new(),
            starting: HashSet::new(),
            cascade_failed: HashSet::new(),
            root_pids: HashMap::new(),
            recovering: HashSet::new(),
            circuit_origin: HashSet::new(),
            restart_events: HashMap::new(),
            recovery_generation: HashMap::new(),
        }
    }
}

impl OrchestratorState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Mark **`name`** and all transitive dependents as **`cascade_failed`** (and clear **starting** / **running** / **root_pid** for those).
pub fn propagate_cascade_failure(
    name: &str,
    dag: &DagRuntime,
    orch: &Mutex<OrchestratorState>,
) {
    let mut q = VecDeque::new();
    q.push_back(name.to_string());
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(n) = q.pop_front() {
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Ok(mut g) = orch.lock() {
            g.cascade_failed.insert(n.clone());
            g.starting.remove(&n);
            g.running.remove(&n);
            g.root_pids.remove(&n);
            g.recovering.remove(&n);
            g.restart_events.remove(&n);
            g.recovery_generation.remove(&n);
        }
        if let Some(children) = dag.dependents.get(&n) {
            for c in children {
                q.push_back(c.clone());
            }
        }
    }
    error!(
        "[SYSX] CASCADE_FAILURE propagated from {:?} — dependents marked Failed/Orphaned",
        name
    );
}

/// Record a single-node failure and propagate (same as cascade from dependency outage).
pub fn record_service_failed(name: &str, dag: &DagRuntime, orch: &Mutex<OrchestratorState>) {
    warn!("[SYSX] ORCHESTRATION_FAILED service={} (readiness/cascade)", name);
    propagate_cascade_failure(name, dag, orch);
}
