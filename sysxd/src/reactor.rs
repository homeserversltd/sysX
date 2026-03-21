//! Main reactor for sysxd - epoll based event loop
//!
//! Handles IPC commands from control socket and cgroup.events monitoring.
//! Core of the supervision loop after pre-reactor DAG validation.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::str;

use log::{error, info};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use sysx_ipc::{
    decode_frame, encode_ipc_reply, peer_allowed_control, validate_peer_credentials, Command,
    Header, SysxCoreBin, OUTCOME_SUCCESS, OUTCOME_UNAUTHORIZED, OUTCOME_WIRE_PARSE, REASON_NA,
    STATUS_OFFLINE, STATUS_REASON_NONE, STATUS_RUNNING, STATUS_SWEEPING,
};
use sysx_runtime::{
    read_sysfs_cgroup_populated, validate_ipc_service_name, RuntimeContext,
};

/// When the request is not decoded, reply with §2.1 outcome bytes using **`Start`** as a neutral
/// carrier (request context unknown). Not on the wire as a real `Start` command.
fn reply_pre_dispatch_error(sock: &mut impl Write, b0: u8, b1: u8) {
    let out = encode_ipc_reply(Command::Start, b0, b1);
    let _ = sock.write_all(&out);
}

use crate::SysXError;

/// Main reactor loop - runs after successful boot and DAG validation (`12` §2.2: `epoll_wait`
/// timeout from sealed `core.bin` for IPC frame assembly bound; `14` §4 / `17` §5 peer policy).
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
    let ev = EpollEvent::new(EpollFlags::EPOLLIN, listen_fd as u64);
    epoll
        .add(&listener, ev)
        .map_err(|e| SysXError::Reactor(format!("epoll add listener: {}", e)))?;

    let epoll_ms = u32::from(sealed.epoll_timeout_ms);
    let epoll_timeout = EpollTimeout::try_from(epoll_ms).map_err(|e| {
        SysXError::Reactor(format!(
            "invalid epoll_timeout_ms {}: {}",
            sealed.epoll_timeout_ms, e
        ))
    })?;

    info!(
        "Starting main reactor (epoll on control socket fd={}, epoll_timeout_ms={}, max_cgroups={})",
        listen_fd, sealed.epoll_timeout_ms, sealed.max_cgroups
    );

    loop {
        let mut events = [EpollEvent::empty(); 16];
        match epoll.wait(&mut events, epoll_timeout) {
            Ok(n) => {
                if n == 0 {
                    continue;
                }

                for event in &events[..n] {
                    let fd = event.data() as RawFd;
                    if fd == listen_fd {
                        accept_and_dispatch(&listener, sealed, rt)?;
                    } else {
                        error!("unexpected epoll data fd {} (only listener registered)", fd);
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

fn accept_and_dispatch(
    listener: &UnixListener,
    sealed: &SysxCoreBin,
    rt: &RuntimeContext,
) -> Result<(), SysXError> {
    loop {
        match listener.accept() {
            Ok((mut sock, _addr)) => {
                let fd = sock.as_raw_fd();

                let creds = match validate_peer_credentials(fd) {
                    Ok(c) => c,
                    Err(e) => {
                        error!("SO_PEERCRED failed: {}", e);
                        reply_pre_dispatch_error(&mut sock, OUTCOME_UNAUTHORIZED, REASON_NA);
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
                    reply_pre_dispatch_error(&mut sock, OUTCOME_UNAUTHORIZED, REASON_NA);
                    continue;
                }

                let mut buf = [0u8; 8192];
                let n = match sock.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        error!("read from control client: {}", e);
                        continue;
                    }
                };
                if n == 0 {
                    continue;
                }

                let (hdr, payload) = match decode_frame(&buf[..n]) {
                    Ok(x) => x,
                    Err(e) => {
                        error!("decode_frame failed: {}", e);
                        reply_pre_dispatch_error(&mut sock, OUTCOME_WIRE_PARSE, REASON_NA);
                        continue;
                    }
                };

                info!("Decoded IPC command {:?}", hdr.command);
                dispatch_ipc(&mut sock, &hdr, payload, sealed, rt);
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

/// `12` §1 / §2.1.1 — `Status` reflects synchronous `cgroup.events` when the service cgroup dir exists.
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
    _sealed: &SysxCoreBin,
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
            info!("Terminal broadcast command {:?} (TODO §9.1)", hdr.command);
            let out = encode_ipc_reply(hdr.command, OUTCOME_SUCCESS, REASON_NA);
            if let Err(e) = sock.write_all(&out) {
                error!("reply write failed: {}", e);
            }
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
        Command::Start | Command::Stop => {
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
