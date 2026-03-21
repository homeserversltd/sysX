//! Main reactor for sysxd - epoll based event loop
//!
//! Handles IPC commands from control socket and cgroup.events monitoring.
//! Core of the supervision loop after pre-reactor DAG validation.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;

use log::{error, info};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use sysx_ipc::{
    decode_frame, encode_ipc_reply, peer_allowed_control, validate_peer_credentials, Command,
    SysxCoreBin, OUTCOME_SUCCESS, OUTCOME_UNAUTHORIZED, REASON_NA, STATUS_REASON_NONE,
    STATUS_RUNNING,
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
pub fn run(listener: UnixListener, sealed: &SysxCoreBin) -> Result<(), SysXError> {
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
                        accept_and_dispatch(&listener, sealed)?;
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

fn accept_and_dispatch(listener: &UnixListener, sealed: &SysxCoreBin) -> Result<(), SysXError> {
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

                let (hdr, _payload) = match decode_frame(&buf[..n]) {
                    Ok(x) => x,
                    Err(e) => {
                        error!("decode_frame failed: {}", e);
                        reply_pre_dispatch_error(&mut sock, 0x03, REASON_NA);
                        continue;
                    }
                };

                info!("Decoded IPC command {:?}", hdr.command);

                let (b0, b1) = match hdr.command {
                    Command::Status => (STATUS_RUNNING, STATUS_REASON_NONE),
                    Command::Poweroff | Command::Reboot => {
                        info!("Terminal broadcast command {:?} (TODO §9.1)", hdr.command);
                        (OUTCOME_SUCCESS, REASON_NA)
                    }
                    Command::Start | Command::Stop | Command::Reset => (OUTCOME_SUCCESS, REASON_NA),
                };

                let out = encode_ipc_reply(hdr.command, b0, b1);
                if let Err(e) = sock.write_all(&out) {
                    error!("reply write failed: {}", e);
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
