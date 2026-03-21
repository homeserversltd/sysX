//! sysxd library interface
//!
//! Re-exports core components for use by other crates and tests.

use thiserror::Error;

pub mod boot;
pub mod cgroup_ops;
pub mod dag;
pub mod reactor;
pub mod schema_load;
pub mod service_spawn;
pub mod stop_ladder;
pub mod terminal_broadcast;

pub use boot::BootState;
pub use boot::initialize as boot_initialize;
pub use dag::validate as validate_dag;
pub use reactor::run as run_reactor;

#[derive(Error, Debug)]
pub enum SysXError {
    #[error("Boot failed: {0}")]
    Boot(String),
    #[error("DAG validation failed: {0}")]
    Dag(String),
    #[error("Reactor error: {0}")]
    Reactor(String),
    #[error("IPC error: {0}")]
    Ipc(#[from] sysx_ipc::FrameError),
}
