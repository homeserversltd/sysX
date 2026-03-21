//! Load `*.yaml` service schemas from **`/etc/sysx/schemas/`** (`15` §DAG, `16` §2).

use std::fs;
use std::path::{Path, PathBuf};

use log::info;

use sysx_schema::{from_yaml, ServiceSchema};

use crate::SysXError;

/// Override with **`SYSX_SCHEMA_DIR`** (tests, initramfs layouts).
pub fn schema_dir() -> PathBuf {
    std::env::var_os("SYSX_SCHEMA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/sysx/schemas"))
}

/// Read all **`*.yaml`** / **`*.yml`** files and parse (`15`).
pub fn load_schemas() -> Result<Vec<ServiceSchema>, SysXError> {
    let dir = schema_dir();
    info!("Loading service schemas from {} (15)", dir.display());
    if !dir.is_dir() {
        info!("Schema directory absent — empty DAG (valid)");
        return Ok(vec![]);
    }
    let mut out: Vec<ServiceSchema> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| {
        SysXError::Dag(format!("read schema dir {}: {}", dir.display(), e))
    })? {
        let entry = entry.map_err(|e| SysXError::Dag(format!("schema dir entry: {}", e)))?;
        let path = entry.path();
        if !is_yaml_file(&path) {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|e| {
            SysXError::Dag(format!("read {}: {}", path.display(), e))
        })?;
        let schema = from_yaml(&text).map_err(|e| {
            SysXError::Dag(format!("parse {}: {}", path.display(), e))
        })?;
        info!(
            "Loaded schema file {} service.name={}",
            path.display(),
            schema.service.name
        );
        out.push(schema);
    }
    Ok(out)
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}
