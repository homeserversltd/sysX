//! Main reactor for sysxd - epoll based event loop
//!
//! Handles IPC commands from control socket and cgroup.events monitoring.
//!
//! `12` §2.2: `epoll_wait` timeout from `core.bin`; partial frame assembly bounded from first byte;
//! `MAX_CONCURRENT_FDS` caps accepted control connections (`14` §5).

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use std::str;
use std::time::{Duration, Instant};

use log::{error, info, warn};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use sysx_ipc::{
    decode_frame, encode_ipc_reply, peer_allowed_control, validate_peer_credentials, Command,
    FrameError, Header, SysxCoreBin, MAX_CONCURRENT_FDS, MAX_PAYLOAD_BYTES,
    OUTCOME_EPISTEMIC_CONFLICT, OUTCOME_SUCCESS, OUTCOME_TOMBSTONED, OUTCOME_UNAUTHORIZED,
    OUTCOME_WIRE_PARSE, REASON_CGROUP_CAPACITY, REASON_NA, STATUS_DEAD_PRIMARY, STATUS_FAILED_PRIMARY,
    STATUS_OFFLINE, STATUS_REASON_CIRCUIT_OPEN, STATUS_REASON_NONE, STATUS_REASON_ORPHANED,
    STATUS_RECOVERING, STATUS_RUNNING, STATUS_SWEEPING, STATUS_TOMBSTONED_PRIMARY,
};
use sysx_schema::ServiceSchema;
use sysx_runtime::{
    read_sysfs_cgroup_populated, read_sysfs_memory_oom_kill_nonzero, validate_ipc_service_name,
    RuntimeContext,
};

use crate::cgroup_ops::{ensure_start_cgroup, StartCgroupError};
use crate::dag::DagRuntime;
use crate::orchestration::{record_service_failed, propagate_cascade_failure, OrchestratorState};
use crate::recovery::{
    backoff_exponential_ms, circuit_should_open, prune_restart_window, recovery_applies,
    recovery_timerfd_arm_ms,
};
use crate::service_spawn::{spawn_simple_service, SpawnOutcome};
use crate::stop_ladder::{
    dfs_post_order_rmdir, is_rmdir_ebusy, stop_service, StopOutcome,
};
use crate::terminal_broadcast::{
    run_terminal_broadcast, TerminalBroadcastKind,
};
#[cfg(target_os = "linux")]
use crate::terminal_broadcast::{on_sigpwr_signalfd, register_sigpwr_signalfd};

use crate::SysXError;

/// Largest valid request frame (`12` §2.2).
const MAX_FRAME_BYTES: usize = Header::SIZE + MAX_PAYLOAD_BYTES;

fn reply_pre_dispatch_error(sock: &mut impl Write, b0: u8, b1: u8) {
    let out = encode_ipc_reply(Command::Start, b0, b1);
    let _ = sock.write_all(&out);
}

fn recovery_timer_lookup(
    pending: &Arc<Mutex<HashMap<String, std::fs::File>>>,
    fd: RawFd,
) -> Option<String> {
    let m = pending.lock().ok()?;
    m.iter()
        .find(|(_, f)| f.as_raw_fd() == fd)
        .map(|(s, _)| s.clone())
}

fn handle_recovery_timer_fd(
    fd: RawFd,
    recovery_pending: &Arc<Mutex<HashMap<String, std::fs::File>>>,
    epoll: &Epoll,
    rt: &RuntimeContext,
    sealed: &SysxCoreBin,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    orch: &Arc<Mutex<OrchestratorState>>,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
) -> Result<(), SysXError> {
    use std::os::unix::io::AsFd;
    use std::io::Read;
    let name = recovery_timer_lookup(recovery_pending, fd).ok_or_else(|| {
        SysXError::Reactor("recovery timer fd not in pending map".into())
    })?;
    let file = {
        let mut m = recovery_pending.lock().map_err(|e| {
            SysXError::Reactor(format!("recovery pending lock: {}", e))
        })?;
        m.remove(&name)
    };
    let Some(mut f) = file else {
        return Ok(());
    };
    let mut drain = [0u8; 8];
    let _ = f.read(&mut drain);
    epoll.delete(f.as_fd()).map_err(|e| {
        SysXError::Reactor(format!("epoll delete recovery timer: {}", e))
    })?;
    drop(f);
    if let Ok(mut g) = orch.lock() {
        g.recovering.remove(&name);
    }
    info!(
        "[SYSX] RECOVERY_TIMER_FIRE service={} — respawn (Phase 4)",
        name
    );
    spawn_service_for_orchestration(
        &name,
        rt,
        sealed,
        tombstoned,
        dfs_proven_dead,
        schemas,
        epoll,
        readiness_files,
        orch,
    )
    .map_err(SysXError::Reactor)?;
    Ok(())
}

struct PendingConn {
    stream: std::os::unix::net::UnixStream,
    buf: Vec<u8>,
    asm_deadline: Option<Instant>,
}

/// Main reactor loop (`12` §2.2, `14` §4–5).
pub fn run(
    listener: UnixListener,
    sealed: &SysxCoreBin,
    rt: &RuntimeContext,
    tombstoned: Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: Arc<Mutex<HashSet<String>>>,
    schemas: Arc<HashMap<String, ServiceSchema>>,
    dag: Arc<DagRuntime>,
    orch: Arc<Mutex<OrchestratorState>>,
    autotest_stop_after_readiness: Option<String>,
) -> Result<(), SysXError> {
    listener
        .set_nonblocking(true)
        .map_err(|e| SysXError::Reactor(format!("listener set_nonblocking: {}", e)))?;

    let epoll = Epoll::new(EpollCreateFlags::empty())
        .map_err(|e| SysXError::Reactor(format!("Failed to create epoll: {}", e)))?;

    let listen_fd = listener.as_raw_fd();
    let listen_ev = EpollEvent::new(EpollFlags::EPOLLIN, listen_fd as u64);
    epoll
        .add(&listener, listen_ev)
        .map_err(|e| SysXError::Reactor(format!("epoll add listener: {}", e)))?;
    let mut listener_in_epoll = true;

    #[cfg(target_os = "linux")]
    let sigpwr: Option<nix::sys::signalfd::SignalFd> = match register_sigpwr_signalfd() {
        Ok(sfd) => {
            info!("SIGPWR signalfd registered for epoll (12 §9.1)");
            Some(sfd)
        }
        Err(e) => {
            warn!("SIGPWR signalfd unavailable: {} — ACPI power path disabled", e);
            None
        }
    };

    #[cfg(target_os = "linux")]
    let signalfd_raw: Option<RawFd> = sigpwr.as_ref().map(|s| s.as_raw_fd());
    #[cfg(not(target_os = "linux"))]
    let signalfd_raw: Option<RawFd> = None;

    #[cfg(target_os = "linux")]
    if let Some(ref sfd) = sigpwr {
        let ev = EpollEvent::new(EpollFlags::EPOLLIN, sfd.as_raw_fd() as u64);
        epoll.add(sfd, ev).map_err(|e| {
            SysXError::Reactor(format!("epoll add SIGPWR signalfd: {}", e))
        })?;
    }

    let mut wake_pipe = [0i32; 2];
    unsafe {
        if libc::pipe(wake_pipe.as_mut_ptr()) != 0 {
            return Err(SysXError::Reactor(format!(
                "reaper wake pipe: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    let wake_r = wake_pipe[0];
    let wake_w = wake_pipe[1];
    unsafe {
        let fl = libc::fcntl(wake_r, libc::F_GETFL);
        if fl >= 0 {
            let _ = libc::fcntl(wake_r, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
    let mut wake_file = unsafe { std::fs::File::from_raw_fd(wake_r) };
    let wake_ev = EpollEvent::new(EpollFlags::EPOLLIN, wake_file.as_raw_fd() as u64);
    epoll
        .add(&wake_file, wake_ev)
        .map_err(|e| SysXError::Reactor(format!("epoll add reaper wake pipe: {}", e)))?;

    let readiness_files: Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let recovery_pending: Arc<Mutex<HashMap<String, std::fs::File>>> =
        Arc::new(Mutex::new(HashMap::new()));

    #[cfg(target_os = "linux")]
    {
        let slice_root = crate::terminal_broadcast::sysx_slice_root(rt);
        let ww = wake_w;
        let _h = sysx_reaper::spawn_slice_observer(slice_root, move |name| {
            let line = format!("{}\n", name);
            if let Err(e) = nix::unistd::write(
                unsafe { BorrowedFd::borrow_raw(ww) },
                line.as_bytes(),
            ) {
                warn!("reaper wake write: {}", e);
            }
        })
        .map_err(|e| SysXError::Reactor(format!("spawn_slice_observer: {}", e)))?;
        let _ = _h;
    }

    let epoll_ms = u32::from(sealed.epoll_timeout_ms);
    let epoll_timeout = EpollTimeout::try_from(epoll_ms).map_err(|e| {
        SysXError::Reactor(format!(
            "invalid epoll_timeout_ms {}: {}",
            sealed.epoll_timeout_ms, e
        ))
    })?;

    let asm_budget = Duration::from_millis(sealed.epoll_timeout_ms as u64);
    let mut clients: HashMap<RawFd, PendingConn> = HashMap::new();

    info!(
        "Starting main reactor (listen fd={}, epoll_timeout_ms={}, max_cgroups={}, max_control_fds={})",
        listen_fd,
        sealed.epoll_timeout_ms,
        sealed.max_cgroups,
        MAX_CONCURRENT_FDS
    );
    info!("SYSX_ORACLE_REACTOR_READY (11 serial oracle)");

    orchestration_bootstrap(
        rt,
        sealed,
        &schemas,
        &dag,
        &orch,
        &tombstoned,
        &dfs_proven_dead,
        &epoll,
        &readiness_files,
    )?;

    let wake_fd = wake_file.as_raw_fd();

    loop {
        poll_assembly_timeouts(
            &epoll,
            &listener,
            listen_fd,
            &mut listener_in_epoll,
            &mut clients,
            sealed,
        )?;

        let mut events = [EpollEvent::empty(); 32];
        match epoll.wait(&mut events, epoll_timeout) {
            Ok(n) => {
                if n == 0 {
                    continue;
                }

                for event in &events[..n] {
                    let fd = event.data() as RawFd;
                    if fd == listen_fd {
                        accept_clients(
                            &epoll,
                            &listener,
                            listen_fd,
                            &mut listener_in_epoll,
                            &mut clients,
                            sealed,
                        )?;
                    } else if signalfd_raw == Some(fd) {
                        #[cfg(target_os = "linux")]
                        if let Some(ref sfd) = sigpwr {
                            on_sigpwr_signalfd(sfd, rt, &orch);
                        }
                    } else if fd == wake_fd {
                        drain_reaper_wake_pipe(
                            &mut wake_file,
                            rt,
                            &dfs_proven_dead,
                            &tombstoned,
                        )?;
                    } else if recovery_timer_lookup(&recovery_pending, fd).is_some() {
                        handle_recovery_timer_fd(
                            fd,
                            &recovery_pending,
                            &epoll,
                            rt,
                            sealed,
                            &tombstoned,
                            &dfs_proven_dead,
                            &schemas,
                            &orch,
                            &readiness_files,
                        )?;
                    } else if readiness_files
                        .lock()
                        .map(|m| m.contains_key(&fd))
                        .unwrap_or(false)
                    {
                        handle_readiness_fd(
                            fd,
                            &readiness_files,
                            &epoll,
                            rt,
                            sealed,
                            &tombstoned,
                            &dfs_proven_dead,
                            &schemas,
                            &dag,
                            &orch,
                            &autotest_stop_after_readiness,
                            &recovery_pending,
                        )?;
                    } else {
                        on_client_readable(
                            &epoll,
                            &listener,
                            listen_fd,
                            &mut listener_in_epoll,
                            fd,
                            &mut clients,
                            &asm_budget,
                            sealed,
                            rt,
                            &tombstoned,
                            &dfs_proven_dead,
                            &schemas,
                            &readiness_files,
                            &dag,
                            &orch,
                        )?;
                    }
                }
            }
            Err(e) => {
                error!("epoll_wait failed: {}", e);
                break;
            }
        }
    }

    info!("Reactor shutdown");
    Ok(())
}

fn maybe_resume_listener(
    epoll: &Epoll,
    listener: &UnixListener,
    listen_fd: RawFd,
    listener_in_epoll: &mut bool,
    active: usize,
) -> Result<(), SysXError> {
    if active < MAX_CONCURRENT_FDS {
        resume_listener(epoll, listener, listen_fd, listener_in_epoll)?;
    }
    Ok(())
}

fn poll_assembly_timeouts(
    epoll: &Epoll,
    listener: &UnixListener,
    listen_fd: RawFd,
    listener_in_epoll: &mut bool,
    clients: &mut HashMap<RawFd, PendingConn>,
    sealed: &SysxCoreBin,
) -> Result<(), SysXError> {
    let now = Instant::now();
    let mut expired: Vec<RawFd> = Vec::new();
    for (fd, conn) in clients.iter() {
        if let Some(dl) = conn.asm_deadline {
            if now > dl && matches!(decode_frame(&conn.buf), Err(FrameError::TruncatedHeader)) {
                error!(
                    "IPC assembly timeout (epoll_timeout_ms={}): fd={} buf_len={}",
                    sealed.epoll_timeout_ms,
                    fd,
                    conn.buf.len()
                );
                expired.push(*fd);
            }
        }
    }
    let need_resume = !expired.is_empty();
    for fd in expired {
        if let Some(mut conn) = remove_client(epoll, clients, fd)? {
            reply_pre_dispatch_error(&mut conn.stream, OUTCOME_WIRE_PARSE, REASON_NA);
        }
    }
    if need_resume {
        maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
    }
    Ok(())
}

fn remove_client(
    epoll: &Epoll,
    clients: &mut HashMap<RawFd, PendingConn>,
    fd: RawFd,
) -> Result<Option<PendingConn>, SysXError> {
    if let Some(conn) = clients.remove(&fd) {
        epoll
            .delete(&conn.stream)
            .map_err(|e| SysXError::Reactor(format!("epoll delete client fd={}: {}", fd, e)))?;
        Ok(Some(conn))
    } else {
        Ok(None)
    }
}

fn suspend_listener(
    epoll: &Epoll,
    listener: &UnixListener,
    listen_fd: RawFd,
    listener_in_epoll: &mut bool,
) -> Result<(), SysXError> {
    if *listener_in_epoll {
        epoll.delete(listener).map_err(|e| {
            SysXError::Reactor(format!("epoll delete listener fd={}: {}", listen_fd, e))
        })?;
        *listener_in_epoll = false;
        info!(
            "control socket accept suspended (active connections == {})",
            MAX_CONCURRENT_FDS
        );
    }
    Ok(())
}

fn resume_listener(
    epoll: &Epoll,
    listener: &UnixListener,
    listen_fd: RawFd,
    listener_in_epoll: &mut bool,
) -> Result<(), SysXError> {
    if !*listener_in_epoll {
        let ev = EpollEvent::new(EpollFlags::EPOLLIN, listen_fd as u64);
        epoll.add(listener, ev).map_err(|e| {
            SysXError::Reactor(format!("epoll re-add listener fd={}: {}", listen_fd, e))
        })?;
        *listener_in_epoll = true;
        info!("control socket accept resumed");
    }
    Ok(())
}

fn accept_clients(
    epoll: &Epoll,
    listener: &UnixListener,
    listen_fd: RawFd,
    listener_in_epoll: &mut bool,
    clients: &mut HashMap<RawFd, PendingConn>,
    sealed: &SysxCoreBin,
) -> Result<(), SysXError> {
    loop {
        if clients.len() >= MAX_CONCURRENT_FDS {
            suspend_listener(epoll, listener, listen_fd, listener_in_epoll)?;
            break;
        }

        match listener.accept() {
            Ok((sock, _addr)) => {
                sock.set_nonblocking(true).map_err(|e| {
                    SysXError::Reactor(format!("client set_nonblocking: {}", e))
                })?;
                let fd = sock.as_raw_fd();

                let creds = match validate_peer_credentials(fd) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("SO_PEERCRED failed: {}", e);
                        let mut s = sock;
                        reply_pre_dispatch_error(&mut s, OUTCOME_UNAUTHORIZED, REASON_NA);
                        continue;
                    }
                };
                if !peer_allowed_control(&creds, sealed.admin_gid) {
                    error!(
                        "control socket peer rejected: uid={} gid={} (need root or gid==admin_gid={})",
                        creds.uid(),
                        creds.gid(),
                        sealed.admin_gid
                    );
                    let mut s = sock;
                    reply_pre_dispatch_error(&mut s, OUTCOME_UNAUTHORIZED, REASON_NA);
                    continue;
                }

                let ev = EpollEvent::new(
                    EpollFlags::EPOLLIN | EpollFlags::EPOLLRDHUP,
                    fd as u64,
                );
                epoll.add(&sock, ev).map_err(|e| {
                    SysXError::Reactor(format!("epoll add client fd={}: {}", fd, e))
                })?;

                clients.insert(
                    fd,
                    PendingConn {
                        stream: sock,
                        buf: Vec::new(),
                        asm_deadline: None,
                    },
                );

                if clients.len() >= MAX_CONCURRENT_FDS {
                    suspend_listener(epoll, listener, listen_fd, listener_in_epoll)?;
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                error!("accept on control socket: {}", e);
                break;
            }
        }
    }
    Ok(())
}

fn on_client_readable(
    epoll: &Epoll,
    listener: &UnixListener,
    listen_fd: RawFd,
    listener_in_epoll: &mut bool,
    fd: RawFd,
    clients: &mut HashMap<RawFd, PendingConn>,
    asm_budget: &Duration,
    sealed: &SysxCoreBin,
    rt: &RuntimeContext,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
    dag: &Arc<DagRuntime>,
    orch: &Arc<Mutex<OrchestratorState>>,
) -> Result<(), SysXError> {
    if clients.get(&fd).is_none() {
        warn!("epoll event for unknown fd {}", fd);
        return Ok(());
    }

    let assembly_dead = {
        let conn = clients.get(&fd).expect("checked");
        if let Some(dl) = conn.asm_deadline {
            Instant::now() > dl
                && matches!(decode_frame(&conn.buf), Err(FrameError::TruncatedHeader))
        } else {
            false
        }
    };
    if assembly_dead {
        error!("IPC assembly deadline exceeded before read fd={}", fd);
        if let Some(mut c) = remove_client(epoll, clients, fd)? {
            reply_pre_dispatch_error(&mut c.stream, OUTCOME_WIRE_PARSE, REASON_NA);
        }
        maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
        return Ok(());
    }

    let mut tmp = [0u8; 2048];
    let n = {
        let conn = clients.get_mut(&fd).expect("present");
        match conn.stream.read(&mut tmp) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => {
                error!("read control client fd={}: {}", fd, e);
                remove_client(epoll, clients, fd)?;
                maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
                return Ok(());
            }
        }
    };

    if n == 0 {
        remove_client(epoll, clients, fd)?;
        maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
        return Ok(());
    }

    {
        let conn = clients.get_mut(&fd).expect("present");
        if conn.buf.is_empty() {
            conn.asm_deadline = Some(Instant::now() + *asm_budget);
        }
        conn.buf.extend_from_slice(&tmp[..n]);
    }

    if clients.get(&fd).expect("present").buf.len() > MAX_FRAME_BYTES {
        error!(
            "IPC frame buffer cap exceeded fd={} len={} (max {})",
            fd,
            clients.get(&fd).expect("present").buf.len(),
            MAX_FRAME_BYTES
        );
        if let Some(mut c) = remove_client(epoll, clients, fd)? {
            reply_pre_dispatch_error(&mut c.stream, OUTCOME_WIRE_PARSE, REASON_NA);
        }
        maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
        return Ok(());
    }

    let dec = {
        let conn = clients.get(&fd).expect("present");
        decode_frame(&conn.buf)
    };

    match dec {
        Ok((hdr, pl)) => {
            let hdr = hdr.clone();
            let pl_owned = pl.to_vec();
            info!("Decoded IPC command {:?}", hdr.command);
            if let Some(mut c) = remove_client(epoll, clients, fd)? {
                dispatch_ipc(
                    &mut c.stream,
                    &hdr,
                    &pl_owned,
                    sealed,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    schemas,
                    epoll,
                    readiness_files,
                    dag,
                    orch,
                );
            }
            maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
        }
        Err(FrameError::TruncatedHeader) => {}
        Err(e) => {
            error!("decode_frame failed fd={}: {}", fd, e);
            if let Some(mut c) = remove_client(epoll, clients, fd)? {
                reply_pre_dispatch_error(&mut c.stream, OUTCOME_WIRE_PARSE, REASON_NA);
            }
            maybe_resume_listener(epoll, listener, listen_fd, listener_in_epoll, clients.len())?;
        }
    }

    Ok(())
}

fn drain_reaper_wake_pipe(
    wake_file: &mut std::fs::File,
    rt: &RuntimeContext,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
) -> Result<(), SysXError> {
    let mut buf = [0u8; 512];
    loop {
        match wake_file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                for line in chunk.lines() {
                    let name = line.trim();
                    if name.is_empty() {
                        continue;
                    }
                    apply_reaper_dfs(rt, name, dfs_proven_dead, tombstoned);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                return Err(SysXError::Reactor(format!("reaper wake read: {}", e)));
            }
        }
    }
    Ok(())
}

fn apply_reaper_dfs(
    rt: &RuntimeContext,
    name: &str,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
) {
    if validate_ipc_service_name(name).is_err() {
        return;
    }
    let path = match rt.service_cgroup_path(name) {
        Ok(p) => p,
        Err(_) => return,
    };
    match dfs_post_order_rmdir(&path) {
        Ok(()) => {
            if let Ok(mut g) = dfs_proven_dead.lock() {
                g.insert(name.to_string());
            }
            if let Ok(mut t) = tombstoned.lock() {
                t.remove(name);
            }
            info!("reaper DFS unlink OK service={} (12 §4.1.1)", name);
        }
        Err(e) if is_rmdir_ebusy(&e) => {
            error!(
                "reaper DFS EBUSY service={} — Tombstoned (topological lock; slot consumed until Reset) (12 §4.1.1)",
                name
            );
            if let Ok(mut t) = tombstoned.lock() {
                t.insert(name.to_string());
            }
        }
        Err(e) => error!("reaper DFS failed service={}: {}", name, e),
    }
}

/// `12` §3 / §4.1.1: **`Dead`** only when cgroup path is **absent** and supervisor has **DFS proof**
/// (`dfs_proven_dead`). While directory exists and `populated=0`, state is **`Sweeping`** (empty-but-pinned).
fn evaluate_status_abi(
    rt: &RuntimeContext,
    name: &str,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    orch: &Arc<Mutex<OrchestratorState>>,
) -> (u8, u8) {
    if let Ok(g) = orch.lock() {
        if g.cascade_failed.contains(name) {
            if g.circuit_origin.contains(name) {
                return (STATUS_FAILED_PRIMARY, STATUS_REASON_CIRCUIT_OPEN);
            }
            return (STATUS_FAILED_PRIMARY, STATUS_REASON_ORPHANED);
        }
        if g.recovering.contains(name) {
            return (STATUS_RECOVERING, STATUS_REASON_NONE);
        }
    }
    let cgroup_dir = match rt.service_cgroup_path(name) {
        Ok(p) => p,
        Err(_) => return (STATUS_OFFLINE, STATUS_REASON_NONE),
    };

    if cgroup_dir.exists() {
        let _ = dfs_proven_dead.lock().map(|mut g| {
            g.remove(name);
        });
        let events_path = match rt.cgroup_events_path(name) {
            Ok(p) => p,
            Err(_) => return (STATUS_OFFLINE, STATUS_REASON_NONE),
        };
        match read_sysfs_cgroup_populated(&events_path) {
            Ok(Some(true)) => (STATUS_RUNNING, STATUS_REASON_NONE),
            Ok(Some(false)) => (STATUS_SWEEPING, STATUS_REASON_NONE),
            Ok(None) => {
                error!(
                    "cgroup.events missing under existing cgroup dir {}",
                    cgroup_dir.display()
                );
                (STATUS_OFFLINE, STATUS_REASON_NONE)
            }
            Err(e) => {
                error!("read cgroup.events {}: {}", events_path.display(), e);
                (STATUS_OFFLINE, STATUS_REASON_NONE)
            }
        }
    } else if dfs_proven_dead
        .lock()
        .map(|g| g.contains(name))
        .unwrap_or(false)
    {
        (STATUS_DEAD_PRIMARY, STATUS_REASON_NONE)
    } else {
        (STATUS_OFFLINE, STATUS_REASON_NONE)
    }
}

fn is_tombstoned(tombstoned: &Arc<Mutex<HashSet<String>>>, name: &str) -> bool {
    tombstoned
        .lock()
        .map(|g| g.contains(name))
        .unwrap_or(false)
}

/// `Command::Status` — `16` §4 **`Tombstoned`**, then `12` §2.1.1 epistemic + sysfs.
fn status_reply(
    rt: &RuntimeContext,
    name: &str,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    orch: &Arc<Mutex<OrchestratorState>>,
) -> (u8, u8) {
    if is_tombstoned(tombstoned, name) {
        return (STATUS_TOMBSTONED_PRIMARY, STATUS_REASON_NONE);
    }
    evaluate_status_abi(rt, name, dfs_proven_dead, orch)
}

fn sysfs_populated_label(rt: &RuntimeContext, name: &str) -> &'static str {
    let events_path = match rt.cgroup_events_path(name) {
        Ok(p) => p,
        Err(_) => return "na",
    };
    match read_sysfs_cgroup_populated(&events_path) {
        Ok(Some(true)) => "1",
        Ok(Some(false)) => "0",
        Ok(None) => "none",
        Err(_) => "err",
    }
}

fn status_sysfs_matches_ipc(
    rt: &RuntimeContext,
    name: &str,
    ipc_b0: u8,
    ipc_b1: u8,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    orch: &Arc<Mutex<OrchestratorState>>,
) -> u8 {
    let k = status_reply(rt, name, tombstoned, dfs_proven_dead, orch);
    u8::from(k.0 == ipc_b0 && k.1 == ipc_b1)
}

/// `11` dual-oracle: serial line for harness / `install.py` (IPC bytes vs sysfs `populated`).
fn log_oracle_dual(
    op: &'static str,
    name: &str,
    hdr_cmd: Command,
    ipc_b0: u8,
    ipc_b1: u8,
    rt: &RuntimeContext,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    orch: &Arc<Mutex<OrchestratorState>>,
) {
    let sysfs_pop = sysfs_populated_label(rt, name);
    if hdr_cmd == Command::Status {
        let m = status_sysfs_matches_ipc(
            rt,
            name,
            ipc_b0,
            ipc_b1,
            tombstoned,
            dfs_proven_dead,
            orch,
        );
        info!(
            "SYSX_ORACLE_DUAL op={} service={} ipc=S:0x{:02x}:R:0x{:02x} sysfs_populated={} sysfs_match={} (11)",
            op, name, ipc_b0, ipc_b1, sysfs_pop, m
        );
    } else {
        info!(
            "SYSX_ORACLE_DUAL op={} service={} ipc=S:0x{:02x}:R:0x{:02x} sysfs_populated={} sysfs_match=na (11)",
            op, name, ipc_b0, ipc_b1, sysfs_pop
        );
    }
}

fn dispatch_ipc(
    sock: &mut impl Write,
    hdr: &Header,
    payload: &[u8],
    sealed: &SysxCoreBin,
    rt: &RuntimeContext,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    epoll: &Epoll,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
    dag: &Arc<DagRuntime>,
    orch: &Arc<Mutex<OrchestratorState>>,
) {
    let plen = payload.len();
    match hdr.command {
        Command::Poweroff | Command::Reboot => {
            if hdr.payload_length != 0 || plen != 0 {
                error!(
                    "{:?} requires empty payload ({} bytes)",
                    hdr.command, plen
                );
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            let (kind, ipc_tag) = if hdr.command == Command::Poweroff {
                (TerminalBroadcastKind::Poweroff, "ipc-0x50")
            } else {
                (TerminalBroadcastKind::Reboot, "ipc-0x60")
            };
            info!(
                "Terminal Broadcast IPC {:?} — reply then §9.1 sequence (trigger={})",
                hdr.command, ipc_tag
            );
            let out = encode_ipc_reply(hdr.command, OUTCOME_SUCCESS, REASON_NA);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
                return;
            }
            run_terminal_broadcast(rt, kind, ipc_tag, orch);
        }
        Command::Reset => {
            if hdr.payload_length == 0 || plen == 0 {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            let name = match str::from_utf8(payload) {
                Ok(s) => s,
                Err(_) => {
                    reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                    return;
                }
            };
            if validate_ipc_service_name(name).is_err() {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            if !is_tombstoned(tombstoned, name) {
                info!(
                    "Reset {:?}: not Tombstoned — epistemic conflict (16 §4)",
                    name
                );
                let out = encode_ipc_reply(hdr.command, OUTCOME_EPISTEMIC_CONFLICT, REASON_NA);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
                }
                log_oracle_dual(
                    "reset",
                    name,
                    Command::Reset,
                    OUTCOME_EPISTEMIC_CONFLICT,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                return;
            }
            let cgroup_path = match rt.service_cgroup_path(name) {
                Ok(p) => p,
                Err(_) => {
                    reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                    return;
                }
            };
            match dfs_post_order_rmdir(&cgroup_path) {
                Ok(()) => {
                    if let Ok(mut g) = tombstoned.lock() {
                        g.remove(name);
                    }
                    if let Ok(mut g) = dfs_proven_dead.lock() {
                        g.remove(name);
                    }
                    info!("Reset {:?}: DFS cleared — Tombstoned cleared (16 §4)", name);
                    let out = encode_ipc_reply(hdr.command, OUTCOME_SUCCESS, REASON_NA);
                    if let Err(e) = sock.write_all(&out) {
                        error!("reply write failed: {}", e);
                    }
                log_oracle_dual(
                    "reset",
                    name,
                    Command::Reset,
                    OUTCOME_SUCCESS,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                }
                Err(e) => {
                    error!("Reset {:?}: DFS unlink failed: {} (EBUSY path 16 §4)", name, e);
                    let out = encode_ipc_reply(hdr.command, OUTCOME_EPISTEMIC_CONFLICT, REASON_NA);
                    if let Err(e) = sock.write_all(&out) {
                        error!("reply write failed: {}", e);
                    }
                    log_oracle_dual(
                        "reset",
                        name,
                        Command::Reset,
                        OUTCOME_EPISTEMIC_CONFLICT,
                        REASON_NA,
                        rt,
                        tombstoned,
                        dfs_proven_dead,
                        orch,
                    );
                }
            }
        }
        Command::Start => {
            if hdr.payload_length == 0 || plen == 0 {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            let name = match str::from_utf8(payload) {
                Ok(s) => s,
                Err(_) => {
                    reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                    return;
                }
            };
            if validate_ipc_service_name(name).is_err() {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            if is_tombstoned(tombstoned, name) {
                info!("Start {:?}: Tombstoned — deny (16 §4)", name);
                let out = encode_ipc_reply(Command::Start, OUTCOME_TOMBSTONED, REASON_NA);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
                }
                log_oracle_dual(
                    "start",
                    name,
                    Command::Start,
                    OUTCOME_TOMBSTONED,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                return;
            }
            if orch
                .lock()
                .map(|g| g.recovering.contains(name))
                .unwrap_or(false)
            {
                info!("Start {:?}: recovering — deny (Phase 4)", name);
                let out = encode_ipc_reply(Command::Start, OUTCOME_EPISTEMIC_CONFLICT, REASON_NA);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
                }
                log_oracle_dual(
                    "start",
                    name,
                    Command::Start,
                    OUTCOME_EPISTEMIC_CONFLICT,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                return;
            }
            if orch
                .lock()
                .map(|g| g.cascade_failed.contains(name))
                .unwrap_or(false)
            {
                info!("Start {:?}: cascade_failed — deny (Phase 3)", name);
                let out = encode_ipc_reply(Command::Start, OUTCOME_EPISTEMIC_CONFLICT, REASON_NA);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
                }
                log_oracle_dual(
                    "start",
                    name,
                    Command::Start,
                    OUTCOME_EPISTEMIC_CONFLICT,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                return;
            }
            if let Err(msg) = orchestration_ipc_start_allowed(name, schemas, dag, &orch) {
                info!("Start {:?}: blocked ({})", name, msg);
                let out = encode_ipc_reply(Command::Start, OUTCOME_EPISTEMIC_CONFLICT, REASON_NA);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
                }
                log_oracle_dual(
                    "start",
                    name,
                    Command::Start,
                    OUTCOME_EPISTEMIC_CONFLICT,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                return;
            }
            let (b0, b1) = match ensure_start_cgroup(rt, name, sealed.max_cgroups) {
                Ok(()) => {
                    if let Some(schema) = schemas.get(name) {
                        match spawn_simple_service(rt, schema) {
                            Ok(outcome) => {
                                if let Ok(mut g) = dfs_proven_dead.lock() {
                                    g.remove(name);
                                }
                                let SpawnOutcome {
                                    readiness_read,
                                    root_pid,
                                } = outcome;
                                let rfd = readiness_read.as_raw_fd();
                                let ev = EpollEvent::new(
                                    EpollFlags::EPOLLIN | EpollFlags::EPOLLRDHUP,
                                    rfd as u64,
                                );
                                if let Ok(mut m) = readiness_files.lock() {
                                    m.insert(rfd, (name.to_string(), readiness_read));
                                    if let Some((_, f)) = m.get(&rfd) {
                                        if let Err(e) = epoll.add(f, ev) {
                                            error!("epoll add readiness fd={}: {}", rfd, e);
                                        }
                                    }
                                }
                                if let Ok(mut g) = orch.lock() {
                                    g.starting.insert(name.to_string());
                                    g.root_pids.insert(name.to_string(), root_pid as i32);
                                }
                                (OUTCOME_SUCCESS, REASON_NA)
                            }
                            Err(e) => {
                                error!("Start {:?}: spawn failed: {}", name, e);
                                (OUTCOME_EPISTEMIC_CONFLICT, REASON_NA)
                            }
                        }
                    } else {
                        if let Ok(mut g) = dfs_proven_dead.lock() {
                            g.remove(name);
                        }
                        (OUTCOME_SUCCESS, REASON_NA)
                    }
                }
                Err(StartCgroupError::AtCapacity { used, max }) => {
                    info!(
                        "Start {:?}: reply [S:{:#04x}, R:{:#04x}] capacity used={} max={} (12 §2.2)",
                        name,
                        OUTCOME_EPISTEMIC_CONFLICT,
                        REASON_CGROUP_CAPACITY,
                        used,
                        max
                    );
                    (OUTCOME_EPISTEMIC_CONFLICT, REASON_CGROUP_CAPACITY)
                }
                Err(StartCgroupError::Mkdir(e)) => {
                    error!("Start {:?}: mkdir: {}", name, e);
                    (OUTCOME_EPISTEMIC_CONFLICT, REASON_NA)
                }
            };
            let out = encode_ipc_reply(Command::Start, b0, b1);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
            log_oracle_dual(
                "start",
                name,
                Command::Start,
                b0,
                b1,
                rt,
                tombstoned,
                dfs_proven_dead,
                orch,
            );
        }
        Command::Stop => {
            if hdr.payload_length == 0 || plen == 0 {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            let name = match str::from_utf8(payload) {
                Ok(s) => s,
                Err(_) => {
                    reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                    return;
                }
            };
            if validate_ipc_service_name(name).is_err() {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            if is_tombstoned(tombstoned, name) {
                info!("Stop {:?}: already Tombstoned (16 §4)", name);
                let out = encode_ipc_reply(Command::Stop, OUTCOME_TOMBSTONED, REASON_NA);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
                }
                log_oracle_dual(
                    "stop",
                    name,
                    Command::Stop,
                    OUTCOME_TOMBSTONED,
                    REASON_NA,
                    rt,
                    tombstoned,
                    dfs_proven_dead,
                    orch,
                );
                return;
            }
            let (b0, b1) = match stop_service(rt, name) {
                Ok(StopOutcome::AlreadyGone) => {
                    info!("Stop {:?}: AlreadyGone (12 §4)", name);
                    if let Ok(mut g) = tombstoned.lock() {
                        g.remove(name);
                    }
                    if let Ok(mut g) = dfs_proven_dead.lock() {
                        g.insert(name.to_string());
                    }
                    if let Ok(mut g) = orch.lock() {
                        g.running.remove(name);
                        g.starting.remove(name);
                        g.root_pids.remove(name);
                    }
                    info!(
                        "[SYSX] ORACLE_DFS_UNLINK_COMPLETE service={} (AlreadyGone)",
                        name
                    );
                    (OUTCOME_SUCCESS, REASON_NA)
                }
                Ok(StopOutcome::Dead) => {
                    info!(
                        "Stop {:?}: Dead after ladder + DFS unlink (12 §4.1 / §4.1.1)",
                        name
                    );
                    if let Ok(mut g) = tombstoned.lock() {
                        g.remove(name);
                    }
                    if let Ok(mut g) = dfs_proven_dead.lock() {
                        g.insert(name.to_string());
                    }
                    if let Ok(mut g) = orch.lock() {
                        g.running.remove(name);
                        g.starting.remove(name);
                        g.root_pids.remove(name);
                    }
                    info!(
                        "[SYSX] ORACLE_DFS_UNLINK_COMPLETE service={} (Dead)",
                        name
                    );
                    (OUTCOME_SUCCESS, REASON_NA)
                }
                Ok(StopOutcome::Tombstoned) => {
                    info!(
                        "Stop {:?}: Tombstoned [S:{:#04x}, R:{:#04x}] (12 §4.1)",
                        name, OUTCOME_TOMBSTONED, REASON_NA
                    );
                    if let Ok(mut g) = tombstoned.lock() {
                        g.insert(name.to_string());
                    }
                    if let Ok(mut g) = orch.lock() {
                        g.running.remove(name);
                        g.starting.remove(name);
                        g.root_pids.remove(name);
                    }
                    (OUTCOME_TOMBSTONED, REASON_NA)
                }
                Err(e) => {
                    error!("Stop {:?}: ladder I/O error: {}", name, e);
                    (OUTCOME_EPISTEMIC_CONFLICT, REASON_NA)
                }
            };
            let out = encode_ipc_reply(Command::Stop, b0, b1);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
            log_oracle_dual(
                "stop",
                name,
                Command::Stop,
                b0,
                b1,
                rt,
                tombstoned,
                dfs_proven_dead,
                orch,
            );
        }
        Command::Status => {
            if hdr.payload_length == 0 || plen == 0 {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            let name = match str::from_utf8(payload) {
                Ok(s) => s,
                Err(_) => {
                    reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                    return;
                }
            };
            if validate_ipc_service_name(name).is_err() {
                reply_pre_dispatch_error(sock, OUTCOME_WIRE_PARSE, REASON_NA);
                return;
            }
            let (b0, b1) = status_reply(rt, name, tombstoned, dfs_proven_dead, orch);
            info!(
                "Status {:?} -> [S:{:#04x}, R:{:#04x}] (12 §2.1.1)",
                name, b0, b1
            );
            let out = encode_ipc_reply(Command::Status, b0, b1);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
            log_oracle_dual(
                "status",
                name,
                Command::Status,
                b0,
                b1,
                rt,
                tombstoned,
                dfs_proven_dead,
                orch,
            );
        }
    }
}

fn orchestration_ipc_start_allowed(
    name: &str,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    dag: &Arc<DagRuntime>,
    orch: &Arc<Mutex<OrchestratorState>>,
) -> Result<(), &'static str> {
    let schema = schemas.get(name).ok_or("unknown service")?;
    let g = orch.lock().map_err(|_| "orch lock poisoned")?;
    for d in &schema.depends_on {
        if g.cascade_failed.contains(d) {
            drop(g);
            propagate_cascade_failure(name, dag, orch);
            return Err("upstream dependency failed");
        }
        if !g.running.contains(d) {
            return Err("upstream not Running");
        }
    }
    Ok(())
}

fn orchestration_bootstrap(
    rt: &RuntimeContext,
    sealed: &SysxCoreBin,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    dag: &Arc<DagRuntime>,
    orch: &Arc<Mutex<OrchestratorState>>,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    epoll: &Epoll,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
) -> Result<(), SysXError> {
    info!(
        "SYSX_ORACLE_BOOTSTRAP dag_services={}",
        dag.boot_order.len()
    );
    try_spawn_ready_services(
        rt,
        sealed,
        tombstoned,
        dfs_proven_dead,
        schemas,
        epoll,
        readiness_files,
        dag,
        orch,
    );
    Ok(())
}

fn try_spawn_ready_services(
    rt: &RuntimeContext,
    sealed: &SysxCoreBin,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    epoll: &Epoll,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
    dag: &Arc<DagRuntime>,
    orch: &Arc<Mutex<OrchestratorState>>,
) {
    for name in &dag.boot_order {
        let skip = orch
            .lock()
            .map(|g| {
                g.running.contains(name)
                    || g.starting.contains(name)
                    || g.recovering.contains(name)
                    || g.cascade_failed.contains(name)
            })
            .unwrap_or(true);
        if skip {
            continue;
        }
        let Some(schema) = schemas.get(name) else {
            continue;
        };
        if !schema.enabled {
            continue;
        }
        let deps_ok = {
            let g = match orch.lock() {
                Ok(x) => x,
                Err(_) => continue,
            };
            let mut ok = true;
            for d in &schema.depends_on {
                if g.cascade_failed.contains(d) {
                    ok = false;
                    break;
                }
                if !g.running.contains(d) {
                    ok = false;
                    break;
                }
            }
            ok
        };
        if !deps_ok {
            continue;
        }
        if let Err(e) = spawn_service_for_orchestration(
            name,
            rt,
            sealed,
            tombstoned,
            dfs_proven_dead,
            schemas,
            epoll,
            readiness_files,
            orch,
        ) {
            warn!("orchestration spawn service={}: {}", name, e);
        }
    }
}

fn spawn_service_for_orchestration(
    name: &str,
    rt: &RuntimeContext,
    sealed: &SysxCoreBin,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    epoll: &Epoll,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
    orch: &Arc<Mutex<OrchestratorState>>,
) -> Result<(), String> {
    if is_tombstoned(tombstoned, name) {
        return Err("tombstoned".into());
    }
    if orch
        .lock()
        .map(|g| g.cascade_failed.contains(name))
        .unwrap_or(false)
    {
        return Err("cascade_failed".into());
    }
    if orch
        .lock()
        .map(|g| {
            g.running.contains(name)
                || g.starting.contains(name)
                || g.recovering.contains(name)
        })
        .unwrap_or(true)
    {
        return Err("already running or starting".into());
    }
    ensure_start_cgroup(rt, name, sealed.max_cgroups).map_err(|e| format!("{:?}", e))?;
    let schema = schemas.get(name).ok_or_else(|| "no schema".to_string())?;
    let outcome = spawn_simple_service(rt, schema).map_err(|e| e.to_string())?;
    if let Ok(mut g) = dfs_proven_dead.lock() {
        g.remove(name);
    }
    let SpawnOutcome {
        readiness_read,
        root_pid,
    } = outcome;
    let rfd = readiness_read.as_raw_fd();
    let ev = EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::EPOLLRDHUP, rfd as u64);
    let mut m = readiness_files
        .lock()
        .map_err(|e| format!("readiness map: {}", e))?;
    m.insert(rfd, (name.to_string(), readiness_read));
    if let Some((_, f)) = m.get(&rfd) {
        epoll.add(f, ev).map_err(|e| format!("epoll add readiness: {}", e))?;
    }
    drop(m);
    if let Ok(mut g) = orch.lock() {
        g.starting.insert(name.to_string());
        g.root_pids.insert(name.to_string(), root_pid as i32);
    }
    info!("orchestration spawned service={} (awaiting FD3)", name);
    Ok(())
}

fn handle_readiness_fd(
    fd: RawFd,
    readiness_files: &Arc<Mutex<HashMap<RawFd, (String, std::fs::File)>>>,
    epoll: &Epoll,
    rt: &RuntimeContext,
    sealed: &SysxCoreBin,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    schemas: &Arc<HashMap<String, ServiceSchema>>,
    dag: &Arc<DagRuntime>,
    orch: &Arc<Mutex<OrchestratorState>>,
    autotest_stop_after_readiness: &Option<String>,
    recovery_pending: &Arc<Mutex<HashMap<String, std::fs::File>>>,
) -> Result<(), SysXError> {
    use std::os::unix::io::AsFd;

    let mut buf = [0u8; 64];
    let mut m = readiness_files
        .lock()
        .map_err(|e| SysXError::Reactor(format!("readiness lock: {}", e)))?;
    let Some((name, file)) = m.get_mut(&fd) else {
        return Ok(());
    };
    let name = name.clone();
    match file.read(&mut buf) {
        Ok(0) => {
            drop(m);
            let file = {
                let mut m2 = readiness_files
                    .lock()
                    .map_err(|e| SysXError::Reactor(format!("readiness lock: {}", e)))?;
                let Some((_, f)) = m2.remove(&fd) else {
                    return Ok(());
                };
                f
            };
            let _ = epoll.delete(file.as_fd());
            if let Ok(mut g) = orch.lock() {
                g.root_pids.remove(&name);
            }
            let oom_kill = match rt.memory_events_path(&name) {
                Ok(p) => read_sysfs_memory_oom_kill_nonzero(&p).unwrap_or(false),
                Err(_) => false,
            };
            if oom_kill {
                info!("[SYSX] ORACLE_OOM_KILL service={}", name);
            }
            let was_starting = orch
                .lock()
                .map(|mut g| {
                    let s = g.starting.remove(&name);
                    let r = g.running.contains(&name);
                    (s, r)
                })
                .unwrap_or((false, false));
            if was_starting.0 && !was_starting.1 {
                let policy = schemas.get(&name).and_then(|s| s.recovery.as_ref());
                if recovery_applies(policy) {
                    let pol = policy.expect("recovery_applies implies policy");
                    let now = Instant::now();
                    let trip = {
                        let mut g = orch.lock().map_err(|e| {
                            SysXError::Reactor(format!("orch lock: {}", e))
                        })?;
                        let events = g.restart_events.entry(name.clone()).or_default();
                        events.push(now);
                        prune_restart_window(events, pol.window_sec, now);
                        circuit_should_open(events, pol.max_restarts)
                    };
                    if trip {
                        if let Ok(mut g) = orch.lock() {
                            g.circuit_origin.insert(name.clone());
                        }
                        propagate_cascade_failure(&name, dag, orch);
                        return Ok(());
                    }
                    let (exp, ms) = {
                        let mut g = orch.lock().map_err(|e| {
                            SysXError::Reactor(format!("orch lock: {}", e))
                        })?;
                        let exp = *g.recovery_generation.entry(name.clone()).or_insert(0);
                        g.recovery_generation
                            .insert(name.clone(), exp.saturating_add(1));
                        g.recovering.insert(name.clone());
                        let ms = backoff_exponential_ms(pol.backoff_ms, exp);
                        (exp, ms)
                    };
                    info!(
                        "[SYSX] RECOVERY_SCHEDULE service={} exp={} backoff_ms={} (Phase 4)",
                        name, exp, ms
                    );
                    let mut pm = recovery_pending.lock().map_err(|e| {
                        SysXError::Reactor(format!("recovery_pending: {}", e))
                    })?;
                    if let Some(old) = pm.remove(&name) {
                        let _ = epoll.delete(old.as_fd());
                    }
                    drop(pm);
                    let tf = recovery_timerfd_arm_ms(ms).map_err(|e| {
                        SysXError::Reactor(format!("recovery_timerfd: {}", e))
                    })?;
                    let ev = EpollEvent::new(EpollFlags::EPOLLIN, tf.as_raw_fd() as u64);
                    epoll.add(&tf, ev).map_err(|e| {
                        SysXError::Reactor(format!("epoll add recovery timer: {}", e))
                    })?;
                    recovery_pending
                        .lock()
                        .map_err(|e| SysXError::Reactor(format!("recovery_pending: {}", e)))?
                        .insert(name.clone(), tf);
                } else {
                    record_service_failed(&name, dag, orch);
                }
            }
        }
        Ok(n) if n > 0 => {
            drop(m);
            let first = orch
                .lock()
                .map(|g| !g.running.contains(&name))
                .unwrap_or(false);
            if first {
                info!(
                    "[SYSX] ORACLE_READINESS service={} bytes={} (FD3)",
                    name, n
                );
                if let Ok(mut g) = orch.lock() {
                    g.starting.remove(&name);
                    g.running.insert(name.clone());
                    g.recovery_generation.remove(&name);
                    g.restart_events.remove(&name);
                }
                try_spawn_ready_services(
                    rt,
                    sealed,
                    tombstoned,
                    dfs_proven_dead,
                    schemas,
                    epoll,
                    readiness_files,
                    dag,
                    orch,
                );
                if autotest_stop_after_readiness.as_deref() == Some(name.as_str()) {
                    autotest_stop_after_readiness_fire(
                        &name,
                        rt,
                        tombstoned,
                        dfs_proven_dead,
                        orch,
                    );
                }
            }
        }
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        Err(e) => warn!("readiness read service={}: {}", name, e),
    }
    Ok(())
}

fn autotest_stop_after_readiness_fire(
    name: &str,
    rt: &RuntimeContext,
    tombstoned: &Arc<Mutex<HashSet<String>>>,
    dfs_proven_dead: &Arc<Mutex<HashSet<String>>>,
    orch: &Arc<Mutex<OrchestratorState>>,
) {
    if is_tombstoned(tombstoned, name) {
        return;
    }
    match stop_service(rt, name) {
        Ok(StopOutcome::Dead) | Ok(StopOutcome::AlreadyGone) => {
            if let Ok(mut g) = orch.lock() {
                g.running.remove(name);
                g.starting.remove(name);
                g.root_pids.remove(name);
            }
            info!(
                "[SYSX] ORACLE_DFS_UNLINK_COMPLETE service={} (autotest stop)",
                name
            );
            if let Ok(mut g) = dfs_proven_dead.lock() {
                g.insert(name.to_string());
            }
        }
        Ok(StopOutcome::Tombstoned) => {
            let _ = orch.lock().map(|mut g| {
                g.running.remove(name);
                g.starting.remove(name);
                g.root_pids.remove(name);
            });
            info!(
                "[SYSX] ORACLE_AUTOTEST_STOP_TOMBSTONED service={}",
                name
            );
        }
        Err(e) => error!("autotest stop service={}: {}", name, e),
    }
}
