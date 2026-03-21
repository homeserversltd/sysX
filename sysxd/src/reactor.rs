//! Main reactor for sysxd - epoll based event loop
//!
//! Handles IPC commands from control socket and cgroup.events monitoring.
//! Core of the supervision loop after pre-reactor DAG validation.

use std::os::unix::io::RawFd;
use log::{info, error};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use sysx_ipc::Command;
use crate::SysXError;

/// Main reactor loop - runs after successful boot and DAG validation
pub fn run() -> Result<(), SysXError> {
    info!("Starting main reactor (epoll_wait on control socket + cgroup.events)");

    // Create epoll instance
    let epoll = Epoll::new(EpollCreateFlags::empty())
        .map_err(|e| SysXError::Reactor(format!("Failed to create epoll: {}", e)))?;

    info!("Reactor initialized - entering epoll_wait loop");

    // Main event loop
    loop {
        let mut events = [EpollEvent::empty(); 16];
        match epoll.wait(&mut events, EpollTimeout::try_from(1000_u32).unwrap()) {
            Ok(n) => {
                if n == 0 {
                    // timeout - could do periodic health checks here
                    continue;
                }

                for event in &events[..n] {
                    handle_event(event)?;
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

/// Handle a single epoll event
fn handle_event(event: &EpollEvent) -> Result<(), SysXError> {
    let fd = event.data() as RawFd;
    let flags = event.events();

    if flags.contains(EpollFlags::EPOLLIN) {
        // Read command from control socket or cgroup.events
        match read_command(fd) {
            Ok(cmd) => {
                info!("Received command: {:?}", cmd);
                process_command(cmd)?;
            }
            Err(e) => {
                error!("Failed to read command: {}", e);
            }
        }
    }

    Ok(())
}

/// Placeholder for reading IPC command from socket
fn read_command(fd: RawFd) -> Result<Command, SysXError> {
    // TODO: implement binary wire protocol parsing from sysx-ipc
    // Should use sysx_ipc::decode_frame()
    Ok(Command::Status)
}

/// Process a received command according to state machine
fn process_command(cmd: Command) -> Result<(), SysXError> {
    match cmd {
        Command::Start => info!("Starting service (TODO)"),
        Command::Stop => info!("Stopping service (TODO)"),
        Command::Status => info!("Status query (TODO)"),
        _ => info!("Command {:?} not yet implemented", cmd),
    }
    Ok(())
}
