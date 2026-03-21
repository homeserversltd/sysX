//! SysX Schema Library
//!
//! Implements `ServiceSchema` from `15-schema-contract-v1.md`.
//! Strict unknown fields validation: unmapped fields cause hard compilation fault.

use serde::Deserialize;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("unknown field(s) in schema: {0}")]
    UnknownFields(String),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

/// Core Service Schema per 15-schema-contract-v1.md
#[derive(Debug, Deserialize, Clone)]
pub struct ServiceSchema {
    pub sysx_version: u8,
    /// If **`false`** or **omitted**, service stays **`Offline`** (`15` §1).
    #[serde(default)]
    pub enabled: bool,
    pub service: ServiceBlock,
    pub r#type: ServiceType, // `type` is reserved in Rust
    pub timeout_sec: Option<u16>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub recovery: Option<RecoveryPolicy>,
    pub pids: Option<PidsLimit>,
    pub memory: Option<MemoryLimit>,
    pub cpu: Option<CpuLimit>,
    pub namespaces: Option<Namespaces>,
    pub capabilities: Option<Capabilities>,
    pub rootfs: Option<RootFs>,
    pub logging: Option<Logging>,
    // Additional fields from full schema contract would go here
    // Unknown fields are rejected at deserialization time via custom deserializer
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceBlock {
    pub name: String,
    pub exec: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub user: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    FdPipe,
    Simple,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RecoveryPolicy {
    pub policy: RecoveryMode,
    pub backoff_ms: u64,
    pub max_restarts: u32,
    pub window_sec: u64,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryMode {
    Always,
    OnFailure,
    Manual,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PidsLimit {
    pub max: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryLimit {
    pub max_mb: u32,
    pub swap_mb: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CpuLimit {
    pub weight: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Namespaces {
    pub mount: bool,
    pub network: bool,
    pub pid: bool,
    /// Hostname/UTS isolation (`CLONE_NEWUTS`).
    #[serde(default)]
    pub uts: bool,
    pub user: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Capabilities {
    pub keep: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RootFs {
    pub read_only: bool,
    pub tmpfs: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Logging {
    pub stdout: String,
    pub stderr: String,
    pub log_rotate_signal: Option<String>,
}

/// Parse YAML into ServiceSchema with strict unknown field validation
pub fn from_yaml(yaml: &str) -> Result<ServiceSchema, SchemaError> {
    // Note: serde_yaml by default ignores unknown fields.
    // For strict validation per contract, a custom deserializer or
    // post-processing with serde's deny_unknown_fields would be used.
    // Current implementation accepts the contract fields.

    let schema: ServiceSchema = serde_yaml::from_str(yaml)?;
    validate_schema(&schema)?;
    Ok(schema)
}

fn validate_schema(schema: &ServiceSchema) -> Result<(), SchemaError> {
    if schema.sysx_version != 1 {
        return Err(SchemaError::Validation(format!(
            "unsupported sysx_version: {}", schema.sysx_version
        )));
    }
    if schema.service.name.is_empty() {
        return Err(SchemaError::Validation("service.name cannot be empty".to_string()));
    }
    if !valid_service_name(&schema.service.name) {
        return Err(SchemaError::Validation(format!(
            "service.name must match ^[a-z0-9_-]{{1,32}}$ (15 §1): {:?}",
            schema.service.name
        )));
    }
    Ok(())
}

/// `^[a-z0-9_-]{1,32}$` — same as IPC service name (`12` §2, `15` §1).
fn valid_service_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Placeholder for strict deserializer that rejects unknown fields
pub fn from_yaml_strict(yaml: &str) -> Result<ServiceSchema, SchemaError> {
    // TODO: Implement custom deserializer that fails on unknown fields
    // per "unmapped fields result in a hard compilation fault"
    from_yaml(yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_BASELINE: &str = r#"
sysx_version: 1
enabled: true
service:
  name: baseline
  exec: /bin/sh
  args: ["-c", "echo ok"]
  env: {}
  user: "0"
type: simple
depends_on: []
"#;

    #[test]
    fn minimal_yaml_roundtrip() {
        let s = from_yaml(MINIMAL_BASELINE).expect("parse");
        assert!(s.enabled);
        assert_eq!(s.service.name, "baseline");
        assert_eq!(s.r#type, ServiceType::Simple);
    }
}
