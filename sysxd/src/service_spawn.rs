//! Fork/exec service workload into cgroup v2 (`12` §3, `15`).
//!
//! **Execution boundary:** parent writes controller files; child **`pre_exec`** migrates via **`cgroup.procs`**,
//! maps **FD 1/2** from schema **`logging`**, **FD 3** readiness pipe write-end, then **`execve`**.
//!
//! ## Red-team: `O_CLOEXEC` vs `execve` (internal audit)
//!
//! Rust **`File`** / **`Command`** may open paths with **`FD_CLOEXEC`**. After **`dup2(old, new)`** onto **1**, **2**, **3**,
//! Linux clears **`FD_CLOEXEC`** on **new** (see **`dup2(2)`**). We still call **`fcntl(fd, F_SETFD, 0)`** on **1**, **2**, **3**
//! so **`execve`** cannot drop stdio/readiness.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use log::{error, info, warn};
use nix::unistd::{close, dup2};
use sysx_schema::ServiceSchema;
use sysx_runtime::RuntimeContext;

/// Parent-held readiness pipe read end — register on **`epoll`** in **`sysxd`**.
pub struct SpawnOutcome {
    pub readiness_read: std::fs::File,
}

fn clear_fd_cloexec(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
}

fn cstring_path(p: &Path) -> Result<CString, io::Error> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn apply_cgroup_limits(rt: &RuntimeContext, schema: &ServiceSchema) -> io::Result<()> {
    let base = rt
        .service_cgroup_path(&schema.service.name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    if let Some(ref p) = schema.pids {
        let path = base.join("pids.max");
        if let Err(e) = fs::write(&path, format!("{}\n", p.max)) {
            warn!("pids.max {}: {}", path.display(), e);
        } else {
            info!("wrote {} = {}", path.display(), p.max);
        }
    }
    if let Some(ref m) = schema.memory {
        let max_bytes = u64::from(m.max_mb) * 1024 * 1024;
        let path = base.join("memory.max");
        if let Err(e) = fs::write(&path, format!("{}\n", max_bytes)) {
            warn!("memory.max {}: {}", path.display(), e);
        } else {
            info!("wrote {} (bytes)", path.display());
        }
    }
    if let Some(ref c) = schema.cpu {
        let path = base.join("cpu.weight");
        if let Err(e) = fs::write(&path, format!("{}\n", c.weight)) {
            warn!("cpu.weight {}: {}", path.display(), e);
        } else {
            info!("wrote {} = {}", path.display(), c.weight);
        }
    }
    Ok(())
}

/// Spawn **`schema.service`**: cgroup limits, **`cgroup.procs`**, logging **FD 1/2**, readiness **FD 3**.
pub fn spawn_simple_service(rt: &RuntimeContext, schema: &ServiceSchema) -> io::Result<SpawnOutcome> {
    if !schema.enabled {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawn skipped: disabled",
        ));
    }

    apply_cgroup_limits(rt, schema)?;

    let procs_path = rt
        .service_cgroup_path(&schema.service.name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?
        .join("cgroup.procs");
    let procs_c = cstring_path(&procs_path)?;

    let exec_path = Path::new(&schema.service.exec);
    if !exec_path.is_file() {
        error!(
            "spawn service={}: exec {} not a file",
            schema.service.name,
            exec_path.display()
        );
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("exec missing: {}", exec_path.display()),
        ));
    }

    let mut fds = [0i32; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let pipe_r = fds[0];
    let pipe_w = fds[1];

    let stdout_path = schema
        .logging
        .as_ref()
        .map(|l| Path::new(&l.stdout).to_path_buf())
        .unwrap_or_else(|| Path::new("/dev/null").to_path_buf());
    let stderr_path = schema
        .logging
        .as_ref()
        .map(|l| Path::new(&l.stderr).to_path_buf())
        .unwrap_or_else(|| Path::new("/dev/null").to_path_buf());

    let out_c = cstring_path(&stdout_path)?;
    let err_c = cstring_path(&stderr_path)?;

    let mut cmd = Command::new(&schema.service.exec);
    cmd.args(&schema.service.args);
    for (k, v) in &schema.service.env {
        cmd.env(k, v);
    }

    unsafe {
        cmd.pre_exec(move || {
            let line = format!("{}\n", libc::getpid());
            let procs_fd = libc::open(procs_c.as_ptr(), libc::O_WRONLY);
            if procs_fd < 0 {
                libc::_exit(1);
            }
            let _ = libc::write(
                procs_fd,
                line.as_ptr() as *const libc::c_void,
                line.len(),
            );
            let _ = libc::close(procs_fd);

            let log_out = libc::open(out_c.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644);
            let log_err = libc::open(err_c.as_ptr(), libc::O_WRONLY | libc::O_CREAT, 0o644);
            if log_out < 0 || log_err < 0 {
                libc::_exit(1);
            }
            if dup2(log_out, 1).is_err() || dup2(log_err, 2).is_err() {
                libc::_exit(1);
            }
            let _ = libc::close(log_out);
            let _ = libc::close(log_err);

            let _ = close(pipe_r);
            if dup2(pipe_w, 3).is_err() {
                libc::_exit(1);
            }
            let _ = close(pipe_w);

            clear_fd_cloexec(1);
            clear_fd_cloexec(2);
            clear_fd_cloexec(3);

            Ok(())
        });
    }

    match cmd.spawn() {
        Ok(ch) => {
            let _ = close(pipe_w);
            let readiness_read = unsafe { std::fs::File::from_raw_fd(pipe_r) };
            info!(
                "spawned service={} child_pid={} exec={}",
                schema.service.name,
                ch.id(),
                schema.service.exec
            );
            Ok(SpawnOutcome { readiness_read })
        }
        Err(e) => {
            let _ = close(pipe_r);
            let _ = close(pipe_w);
            error!("spawn service={}: {}", schema.service.name, e);
            Err(e)
        }
    }
}
