//! SysX Compiler
//!
//! Compiles service YAML to ServiceSchema and forges the 32-byte SysxCoreConfig
//! sealed boot artifact per 17-sysx-sealed-boot-core-bin.md §4 and 15-schema-contract.

use std::fs;
use sysx_schema::{ServiceSchema, from_yaml_strict, SchemaError};
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

/// 32-byte SysxCoreConfig sealed at forge time (per 17 §4)
#[derive(Debug, Clone)]
pub struct SysxCoreConfig {
    pub admin_gid: u32,
    pub watchdog_timeout_ms: u32,
    pub max_payload_bytes: u32,
    pub max_concurrent_fds: u16,
    pub epoll_timeout_ms: u16,
    pub schema_version: u8,
    // 32-byte fixed size padding/reserved
    _padding: [u8; 7],
}

impl SysxCoreConfig {
    pub fn new(admin_gid: u32) -> Self {
        Self {
            admin_gid,
            watchdog_timeout_ms: 10000,
            max_payload_bytes: 4096,
            max_concurrent_fds: 64,
            epoll_timeout_ms: 100,
            schema_version: 1,
            _padding: [0; 7],
        }
    }

    /// Forge core.bin (32-byte binary artifact)
    pub fn forge(&self, path: &str) -> Result<(), CompilerError> {
        let mut buf = Vec::with_capacity(32);

        // Pack into 32 bytes (simple layout for PID1 zero-allocation read)
        buf.extend_from_slice(&self.admin_gid.to_le_bytes());
        buf.extend_from_slice(&self.watchdog_timeout_ms.to_le_bytes());
        buf.extend_from_slice(&self.max_payload_bytes.to_le_bytes());
        buf.extend_from_slice(&self.max_concurrent_fds.to_le_bytes());
        buf.extend_from_slice(&self.epoll_timeout_ms.to_le_bytes());
        buf.push(self.schema_version);
        buf.extend_from_slice(&self._padding);

        // Ensure exactly 32 bytes
        while buf.len() < 32 {
            buf.push(0);
        }
        if buf.len() > 32 {
            buf.truncate(32);
        }

        fs::write(path, &buf)?;
        Ok(())
    }
}

/// Compile a service YAML file to validated ServiceSchema
pub fn compile_service(yaml_path: &str) -> Result<ServiceSchema, CompilerError> {
    let yaml = fs::read_to_string(yaml_path)?;
    let schema = from_yaml_strict(&yaml)?;
    Ok(schema)
}

/// Forge sealed core.bin for PID 1 bootstrap
pub fn forge_core_bin(output_path: &str, admin_gid: u32) -> Result<(), CompilerError> {
    let config = SysxCoreConfig::new(admin_gid);
    config.forge(output_path)?;
    Ok(())
}
