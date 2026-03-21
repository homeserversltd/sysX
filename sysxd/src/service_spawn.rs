//! Fork/exec service workload into cgroup v2 (`12` §3, `15` simple services).
//!
//! Joins cgroup by writing child PID to **`cgroup.procs`** in a **`pre_exec`** hook before **`exec`**.

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use log::{error, info};
use sysx_schema::ServiceSchema;
use sysx_runtime::RuntimeContext;

/// Spawn **`schema.service`** into **`rt`** cgroup (`cgroup.procs` migration in child pre-exec).
pub fn spawn_simple_service(rt: &RuntimeContext, schema: &ServiceSchema) -> io::Result<()> {
    if !schema.enabled {
        info!(
            "spawn skipped (disabled) service={}",
            schema.service.name
        );
        return Ok(());
    }

    let procs_path = rt
        .service_cgroup_path(&schema.service.name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?
        .join("cgroup.procs");

    let exec_path = Path::new(&schema.service.exec);
    if !exec_path.is_file() {
        error!(
            "spawn service={}: exec {} not a file (rootfs incomplete?)",
            schema.service.name,
            exec_path.display()
        );
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("exec missing: {}", exec_path.display()),
        ));
    }

    let mut cmd = Command::new(&schema.service.exec);
    cmd.args(&schema.service.args);
    for (k, v) in &schema.service.env {
        cmd.env(k, v);
    }

    let procs_clone = procs_path.clone();
    unsafe {
        cmd.pre_exec(move || {
            let pid = libc::getpid();
            let line = format!("{}\n", pid);
            if fs::write(&procs_clone, line.as_bytes()).is_err() {
                libc::_exit(1);
            }
            Ok(())
        });
    }

    match cmd.spawn() {
        Ok(ch) => {
            info!(
                "spawned service={} child_pid={} exec={}",
                schema.service.name,
                ch.id(),
                schema.service.exec
            );
            Ok(())
        }
        Err(e) => {
            error!("spawn service={}: {}", schema.service.name, e);
            Err(e)
        }
    }
}
