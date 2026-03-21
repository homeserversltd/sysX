//! sysxd library interface
//!
//! Re-exports core components for use by other crates and tests.

pub mod boot;
pub mod dag;
pub mod reactor;

pub use boot::initialize as boot_initialize;
pub use dag::validate as validate_dag;
pub use reactor::run as run_reactor;

pub use crate::SysXError;
