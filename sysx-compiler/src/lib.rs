//! SysX Compiler
//!
//! Compiles service YAML to ServiceSchema and forges the 32-byte sealed boot artifact
//! per `17-sysx-sealed-boot-core-bin.md` §4 and `15-schema-contract-v1.md`.

use std::fs;
use sysx_ipc::encode_core_bin;
use sysx_schema::{from_yaml_strict, SchemaError, ServiceSchema};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("forge error: {0}")]
    Forge(String),
}

/// Default supervised cgroup capacity when forge CLI does not pass overrides (`12` §2.2).
pub const DEFAULT_MAX_CGROUPS: u32 = 256;

/// Default `epoll_wait` timeout for IPC frame assembly (milliseconds, `12` §2.2).
pub const DEFAULT_EPOLL_TIMEOUT_MS: u16 = 100;

/// Forge sealed `core.bin` for PID 1 bootstrap (canonical `17` §4 layout via `sysx-ipc`).
pub fn forge_core_bin(output_path: &str, admin_gid: u32) -> Result<(), CompilerError> {
    forge_core_bin_full(
        output_path,
        admin_gid,
        DEFAULT_MAX_CGROUPS,
        DEFAULT_EPOLL_TIMEOUT_MS,
    )
}

/// Full forge with explicit reactor fields (`12` §2.2).
pub fn forge_core_bin_full(
    output_path: &str,
    admin_gid: u32,
    max_cgroups: u32,
    epoll_timeout_ms: u16,
) -> Result<(), CompilerError> {
    let raw = encode_core_bin(admin_gid, max_cgroups, epoll_timeout_ms);
    fs::write(output_path, raw.as_slice()).map_err(CompilerError::Io)?;
    Ok(())
}

/// Compile a service YAML file to validated ServiceSchema
pub fn compile_service(yaml_path: &str) -> Result<ServiceSchema, CompilerError> {
    let yaml = fs::read_to_string(yaml_path)?;
    let schema = from_yaml_strict(&yaml)?;
    Ok(schema)
}
