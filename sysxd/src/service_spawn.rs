//! Fork/exec service workload into cgroup v2 (`12` §3, `15`).
//!
//! **Execution boundary:** parent writes controller files; child **`pre_exec`** migrates via **`cgroup.procs`**,
//! optional **namespace / mount / capability** isolation (Phase 2), then maps **FD 1/2** from **`logging`**,
//! **FD 3** readiness pipe write-end, then **`execve`**.
//!
//! ## Red-team: `O_CLOEXEC` vs `execve` (internal audit)
//!
//! Rust **`File`** / **`Command`** may open paths with **`FD_CLOEXEC`**. After **`dup2(old, new)`** onto **1**, **2**, **3**,
//! Linux clears **`FD_CLOEXEC`** on **new** (see **`dup2(2)`**). We still call **`fcntl(fd, F_SETFD, 0)`** on **1**, **2**, **3**
//! so **`execve`** cannot drop stdio/readiness.
//!
//! ## Red-team: Immutability Matrix mount **ordering** (Step 2)
//!
//! When **`CLONE_NEWNS`** is used, mounts apply in this order:
//! 1. **`MS_REC | MS_PRIVATE`** on **`/`** — propagation severance only; does not make the root mount read-only.
//! 2. **Ephemeral `tmpfs`** mounts — each target path is **`create_dir_all`** first, then **`tmpfs`** mounted **writable**.
//!    Doing tmpfs **before** the root read-only remount guarantees each tmpfs is its **own** mount tree; the subsequent
//!    **`MS_REMOUNT | MS_BIND | MS_RDONLY`** only affects the **root** mount, not separate mounts that sit **on top**
//!    of pathnames under `/`. Reversing the order (ro-remount **first**) can make mkdir/mount fail on read-only parents,
//!    or cause **`EBUSY`** when tmpfs must attach under a locked tree.
//! 3. **`MS_REMOUNT | MS_BIND | MS_RDONLY`** on **`/`** — locks the **root** filesystem read-only; **tmpfs** mounts
//!    from step 2 remain writable because they are distinct superblocks.
//!
//! ## Phase 2: `CLONE_NEWPID` / `CLONE_NEWUSER`
//!
//! **`std::process::Command::pre_exec`** runs in the **single** child that must **`execve`**. Linux **`unshare(CLONE_NEWPID)`**
//! leaves the caller in the old PID namespace; only the **next forked child** becomes PID 1 in the new namespace.
//! Applying **`CLONE_NEWPID`** correctly requires a **fork/exec** chain outside this API. Schema **`namespaces.pid`** is
//! recognized but **not** OR'd into **`unshare(2)`** here until spawn is refactored to **`clone(2)`** or a double-fork
//! wrapper. **`CLONE_NEWUSER`** is likewise deferred (ordering and uid maps).

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

#[cfg(target_os = "linux")]
use nix::mount::{mount, MsFlags};

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

/// Build `unshare(2)` flags from schema. **`CLONE_NEWPID`** / **`CLONE_NEWUSER`** omitted — see module doc.
#[cfg(target_os = "linux")]
fn unshare_flags_from_schema(schema: &ServiceSchema) -> libc::c_int {
    let Some(ref ns) = schema.namespaces else {
        return 0;
    };
    let mut f = 0;
    if ns.mount {
        f |= libc::CLONE_NEWNS;
    }
    if ns.network {
        f |= libc::CLONE_NEWNET;
    }
    if ns.uts {
        f |= libc::CLONE_NEWUTS;
    }
    let _ = ns.pid;
    let _ = ns.user;
    f
}

/// Raise loopback in a new network namespace (`SIOCGIFFLAGS` / `SIOCSIFFLAGS` on **`lo`**).
#[cfg(target_os = "linux")]
unsafe fn lo_interface_up() {
    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
    let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
    if sock < 0 {
        return;
    }
    let mut ifr: [u8; 40] = [0; 40];
    let lo = b"lo\0";
    ifr[..lo.len()].copy_from_slice(lo);
    if libc::ioctl(sock, SIOCGIFFLAGS, ifr.as_mut_ptr()) < 0 {
        let _ = libc::close(sock);
        return;
    }
    let fl = i16::from_ne_bytes([ifr[16], ifr[17]]);
    let new_fl = fl | (libc::IFF_UP as i16) | (libc::IFF_LOOPBACK as i16);
    ifr[16..18].copy_from_slice(&new_fl.to_ne_bytes());
    let _ = libc::ioctl(sock, SIOCSIFFLAGS, ifr.as_mut_ptr());
    let _ = libc::close(sock);
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: libc::c_int,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[cfg(target_os = "linux")]
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Map **`CAP_*`** name or decimal index to capability bit (`0`..=`40` on Linux ≤5.x).
#[cfg(target_os = "linux")]
fn capability_bit(name: &str) -> Option<u32> {
    let s = name.trim();
    if let Ok(n) = s.parse::<u32>() {
        return if n <= 40 { Some(n) } else { None };
    }
    let u = s.strip_prefix("CAP_").unwrap_or(s);
    match u.to_ascii_uppercase().as_str() {
        "CHOWN" => Some(0),
        "DAC_OVERRIDE" => Some(1),
        "DAC_READ_SEARCH" => Some(2),
        "FOWNER" => Some(3),
        "FSETID" => Some(4),
        "KILL" => Some(5),
        "SETGID" => Some(6),
        "SETUID" => Some(7),
        "SETPCAP" => Some(8),
        "LINUX_IMMUTABLE" => Some(9),
        "NET_BIND_SERVICE" => Some(10),
        "NET_BROADCAST" => Some(11),
        "NET_ADMIN" => Some(12),
        "NET_RAW" => Some(13),
        "IPC_LOCK" => Some(14),
        "IPC_OWNER" => Some(15),
        "SYS_MODULE" => Some(16),
        "SYS_RAWIO" => Some(17),
        "SYS_CHROOT" => Some(18),
        "SYS_PTRACE" => Some(19),
        "SYS_PACCT" => Some(20),
        "SYS_ADMIN" => Some(21),
        "SYS_BOOT" => Some(22),
        "SYS_NICE" => Some(23),
        "SYS_RESOURCE" => Some(24),
        "SYS_TIME" => Some(25),
        "SYS_TTY_CONFIG" => Some(26),
        "MKNOD" => Some(27),
        "LEASE" => Some(28),
        "AUDIT_WRITE" => Some(29),
        "AUDIT_CONTROL" => Some(30),
        "SETFCAP" => Some(31),
        "MAC_OVERRIDE" => Some(32),
        "MAC_ADMIN" => Some(33),
        "SYSLOG" => Some(34),
        "WAKE_ALARM" => Some(35),
        "BLOCK_SUSPEND" => Some(36),
        "AUDIT_READ" => Some(37),
        "PERFMON" => Some(38),
        "BPF" => Some(39),
        "CHECKPOINT_RESTORE" => Some(40),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
unsafe fn linux_apply_capset(keep: &[String]) -> Result<(), ()> {
    let mut eff0 = 0u32;
    let mut eff1 = 0u32;
    for s in keep {
        let bit = capability_bit(s).ok_or(())?;
        if bit < 32 {
            eff0 |= 1u32 << bit;
        } else {
            eff1 |= 1u32 << (bit - 32);
        }
    }
    let mut hdr = CapUserHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapUserData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapUserData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    let g = libc::syscall(
        libc::SYS_capget,
        &mut hdr as *mut CapUserHeader,
        data.as_mut_ptr(),
    );
    if g != 0 {
        return Err(());
    }
    data[0].effective = eff0;
    data[0].permitted = eff0;
    data[0].inheritable = 0;
    data[1].effective = eff1;
    data[1].permitted = eff1;
    data[1].inheritable = 0;
    let s = libc::syscall(
        libc::SYS_capset,
        &mut hdr as *mut CapUserHeader,
        data.as_ptr(),
    );
    if s != 0 {
        return Err(());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

#[cfg(target_os = "linux")]
unsafe fn linux_apply_privilege_scythe(schema: &ServiceSchema) {
    if let Some(ref caps) = schema.capabilities {
        if !caps.keep.is_empty() {
            if let Err(()) = linux_apply_capset(&caps.keep) {
                libc::_exit(88);
            }
        }
    }
    if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        libc::_exit(89);
    }
    let Ok(uid) = schema.service.user.trim().parse::<libc::uid_t>() else {
        return;
    };
    if uid == 0 {
        return;
    }
    if libc::setgid(uid) != 0 {
        libc::_exit(90);
    }
    if libc::setuid(uid) != 0 {
        libc::_exit(91);
    }
}

#[cfg(target_os = "linux")]
unsafe fn linux_apply_mount_matrix(schema: &ServiceSchema, did_mount_ns: bool) {
    if !did_mount_ns {
        return;
    }
    let root = Path::new("/");
    if mount::<str, _, str, str>(
        None,
        root,
        None,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None,
    )
    .is_err()
    {
        libc::_exit(80);
    }

    if let Some(ref rf) = schema.rootfs {
        if let Some(ref m) = rf.tmpfs {
            let mut keys: Vec<String> = m.keys().cloned().collect();
            keys.sort();
            for target in keys {
                let opts = m.get(&target).map(|s| s.as_str()).unwrap_or("size=64M,mode=1777");
                let path = Path::new(&target);
                if fs::create_dir_all(path).is_err() {
                    libc::_exit(81);
                }
                if mount(
                    Some("tmpfs"),
                    path,
                    Some("tmpfs"),
                    MsFlags::empty(),
                    Some(opts),
                )
                .is_err()
                {
                    libc::_exit(82);
                }
            }
        }
        if rf.read_only {
            if mount::<str, _, str, str>(
                None,
                root,
                None,
                MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
                None,
            )
            .is_err()
            {
                libc::_exit(83);
            }
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn child_pre_exec_linux(
    procs_c: &CString,
    out_c: &CString,
    err_c: &CString,
    pipe_r: RawFd,
    pipe_w: RawFd,
    schema: &ServiceSchema,
) {
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

    let flags = unshare_flags_from_schema(schema);
    let want_mount_ns = schema.namespaces.as_ref().map(|n| n.mount).unwrap_or(false);
    if flags != 0 && libc::unshare(flags) != 0 {
        libc::_exit(77);
    }
    let did_mount_ns = want_mount_ns && flags & libc::CLONE_NEWNS != 0;

    linux_apply_mount_matrix(schema, did_mount_ns);

    if schema.namespaces.as_ref().map(|n| n.network).unwrap_or(false) && (flags & libc::CLONE_NEWNET != 0) {
        lo_interface_up();
    }

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

    linux_apply_privilege_scythe(schema);
}

#[cfg(not(target_os = "linux"))]
unsafe fn child_pre_exec_linux(
    procs_c: &CString,
    out_c: &CString,
    err_c: &CString,
    pipe_r: RawFd,
    pipe_w: RawFd,
    _schema: &ServiceSchema,
) {
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
}

/// Spawn **`schema.service`**: cgroup limits, **`cgroup.procs`**, Phase 2 isolation (Linux), logging **FD 1/2**, readiness **FD 3**.
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

    let schema_clone = schema.clone();

    unsafe {
        cmd.pre_exec(move || {
            child_pre_exec_linux(
                &procs_c,
                &out_c,
                &err_c,
                pipe_r,
                pipe_w,
                &schema_clone,
            );
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
