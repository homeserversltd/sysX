//! Main reactor for sysxd - epoll based event loop
//!
//! Handles IPC commands from control socket and cgroup.events monitoring.
//!
//! `12` §2.2: `epoll_wait` timeout from `core.bin`; partial frame assembly bounded from first byte;
//! `MAX_CONCURRENT_FDS` caps accepted control connections (`14` §5).

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;

use std::str;
use std::time::{Duration, Instant};

use log::{error, info, warn};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use sysx_ipc::{
    decode_frame, encode_ipc_reply, peer_allowed_control, validate_peer_credentials, Command,
    FrameError, Header, SysxCoreBin, MAX_CONCURRENT_FDS, MAX_PAYLOAD_BYTES,
    OUTCOME_EPISTEMIC_CONFLICT, OUTCOME_SUCCESS, OUTCOME_UNAUTHORIZED, OUTCOME_WIRE_PARSE,
    REASON_CGROUP_CAPACITY, REASON_NA, STATUS_OFFLINE, STATUS_REASON_NONE, STATUS_RUNNING,
    STATUS_SWEEPING,
};
use sysx_runtime::{
    read_sysfs_cgroup_populated, validate_ipc_service_name, RuntimeContext,
};

use crate::cgroup_ops::{ensure_start_cgroup, StartCgroupError};
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
                            on_sigpwr_signalfd(sfd, rt);
                        }
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
                dispatch_ipc(&mut c.stream, &hdr, &pl_owned, sealed, rt);
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

fn status_from_kernel(rt: &RuntimeContext, name: &str) -> (u8, u8) {
    let cgroup_dir = match rt.service_cgroup_path(name) {
        Ok(p) => p,
        Err(_) => return (STATUS_OFFLINE, STATUS_REASON_NONE),
    };
    if !cgroup_dir.exists() {
        return (STATUS_OFFLINE, STATUS_REASON_NONE);
    }
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
}

fn dispatch_ipc(
    sock: &mut impl Write,
    hdr: &Header,
    payload: &[u8],
    sealed: &SysxCoreBin,
    rt: &RuntimeContext,
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
            run_terminal_broadcast(rt, kind, ipc_tag);
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
            let out = encode_ipc_reply(hdr.command, OUTCOME_SUCCESS, REASON_NA);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
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
            let (b0, b1) = match ensure_start_cgroup(rt, name, sealed.max_cgroups) {
                Ok(()) => (OUTCOME_SUCCESS, REASON_NA),
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
                Err(StartCgroupError::Mkdir(_)) => {
                    (OUTCOME_EPISTEMIC_CONFLICT, REASON_NA)
                }
            };
            let out = encode_ipc_reply(Command::Start, b0, b1);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
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
            let out = encode_ipc_reply(hdr.command, OUTCOME_SUCCESS, REASON_NA);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
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
            let (b0, b1) = status_from_kernel(rt, name);
            info!(
                "Status {:?} -> [S:{:#04x}, R:{:#04x}] (kernel sysfs)",
                name, b0, b1
            );
            let out = encode_ipc_reply(Command::Status, b0, b1);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
        }
    }
}
