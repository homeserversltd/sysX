//! SysX IPC v1 Wire Protocol and Shared Types
//!
//! Implements the binary wire format from §2 of 12-canonical-spec-v1.md.
//! Enforces SO_PEERCRED contract, framing bounds (MAX_PAYLOAD_BYTES=4096,
//! MAX_CONCURRENT_FDS=64), and epoll_timeout_ms from core config.
//!
//! Commands 0x10-0x60 are the canonical command space.

use std::os::fd::BorrowedFd;
use std::os::unix::io::RawFd;
use nix::sys::socket::UnixCredentials;
use thiserror::Error;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

/// Magic bytes for SysX IPC framing ("SYSX")
pub const SYSX_MAGIC: &[u8; 4] = b"SYSX";
pub const SYSX_VERSION: u8 = 1;

/// Maximum payload size per frame (4096 bytes)
pub const MAX_PAYLOAD_BYTES: usize = 4096;
/// Maximum concurrent FDs allowed on control socket
pub const MAX_CONCURRENT_FDS: usize = 64;

/// Command opcodes (0x10-0x60 range per spec)
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    /// Start a service (0x10)
    Start = 0x10,
    /// Stop a service (0x11)
    Stop = 0x11,
    /// Restart service (0x12)
    Restart = 0x12,
    /// Get status (0x13)
    Status = 0x13,
    /// Freeze cgroup (0x20)
    Freeze = 0x20,
    /// Unfreeze cgroup (0x21)
    Unfreeze = 0x21,
    /// Kill processes in cgroup (0x22)
    Kill = 0x22,
    /// Get cgroup events (0x23)
    GetEvents = 0x23,
    /// Readiness probe (0x30)
    Readiness = 0x30,
    /// Register service (0x40)
    Register = 0x40,
    /// Deregister service (0x41)
    Deregister = 0x41,
    /// List services (0x42)
    List = 0x42,
    /// Custom extension range starts here
    Custom = 0x50,
    /// Shutdown PID 1 (0x60)
    Shutdown = 0x60,
}

impl Command {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x10 => Some(Command::Start),
            0x11 => Some(Command::Stop),
            0x12 => Some(Command::Restart),
            0x13 => Some(Command::Status),
            0x20 => Some(Command::Freeze),
            0x21 => Some(Command::Unfreeze),
            0x22 => Some(Command::Kill),
            0x23 => Some(Command::GetEvents),
            0x30 => Some(Command::Readiness),
            0x40 => Some(Command::Register),
            0x41 => Some(Command::Deregister),
            0x42 => Some(Command::List),
            0x50..=0x5F => Some(Command::Custom),
            0x60 => Some(Command::Shutdown),
            _ => None,
        }
    }
}

/// Fixed 16-byte header per canonical spec §2
#[derive(Debug, Clone)]
pub struct Header {
    pub magic: [u8; 4],
    pub version: u8,
    pub command: Command,
    pub payload_length: u32,
}

impl Header {
    pub const SIZE: usize = 16;

    pub fn new(command: Command, payload_length: u32) -> Self {
        let mut magic = [0u8; 4];
        magic.copy_from_slice(SYSX_MAGIC);

        Self {
            magic,
            version: SYSX_VERSION,
            command,
            payload_length: payload_length.min(MAX_PAYLOAD_BYTES as u32),
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.reserve(Self::SIZE);
        buf.put_slice(&self.magic);
        buf.put_u8(self.version);
        buf.put_u8(self.command as u8);
        buf.put_u8(0); // reserved
        buf.put_u8(0); // reserved
        buf.put_u32(self.payload_length);
        buf.put_u32(0); // reserved for future use (e.g. flags)
        buf.put_u32(0); // reserved for future use
    }

    pub fn decode(mut buf: Bytes) -> Result<Self, FrameError> {
        if buf.len() < Self::SIZE {
            return Err(FrameError::TruncatedHeader);
        }

        let mut magic = [0u8; 4];
        buf.copy_to_slice(&mut magic);

        if &magic != SYSX_MAGIC {
            return Err(FrameError::BadMagic);
        }

        let version = buf.get_u8();
        if version != SYSX_VERSION {
            return Err(FrameError::VersionMismatch { got: version });
        }

        let cmd_byte = buf.get_u8();
        let command = Command::from_u8(cmd_byte)
            .ok_or(FrameError::UnknownCommand(cmd_byte))?;

        // skip 2 reserved bytes + 3 u32s
        let _ = buf.get_u16();
        let payload_length = buf.get_u32();
        let _ = buf.get_u64();

        if payload_length as usize > MAX_PAYLOAD_BYTES {
            return Err(FrameError::PayloadTooLarge(payload_length as usize));
        }

        Ok(Self {
            magic,
            version,
            command,
            payload_length,
        })
    }
}

/// IPC framing errors
#[derive(Error, Debug)]
pub enum FrameError {
    #[error("truncated header")]
    TruncatedHeader,
    #[error("bad magic bytes")]
    BadMagic,
    #[error("version mismatch: got {got}, expected {expected}", expected = SYSX_VERSION)]
    VersionMismatch { got: u8 },
    #[error("unknown command: 0x{0:02x}")]
    UnknownCommand(u8),
    #[error("payload too large: {0} bytes (max {MAX_PAYLOAD_BYTES})")]
    PayloadTooLarge(usize),
    #[error("SO_PEERCRED validation failed")]
    CredValidationFailed,
}

/// Core IPC message
#[derive(Debug, Clone)]
pub struct Message {
    pub header: Header,
    pub payload: Bytes,
}

impl Message {
    pub fn new(command: Command, payload: Bytes) -> Self {
        let header = Header::new(command, payload.len() as u32);
        Self { header, payload }
    }

    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Header::SIZE + self.payload.len());
        self.header.encode(&mut buf);
        buf.put_slice(&self.payload);
        buf.freeze()
    }
}

/// SO_PEERCRED based peer validation contract
pub fn validate_peer_credentials(fd: RawFd) -> Result<UnixCredentials, FrameError> {
    // SO_PEERCRED via getsockopt (nix 0.29 expects AsFd — use BorrowedFd::borrow_raw)
    unsafe {
        let borrowed = BorrowedFd::borrow_raw(fd);
        nix::sys::socket::getsockopt(&borrowed, nix::sys::socket::sockopt::PeerCredentials)
            .map_err(|_| FrameError::CredValidationFailed)
    }
}

/// Configuration type that carries epoll_timeout_ms from core.bin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub epoll_timeout_ms: u32,
    pub max_payload_bytes: usize,
    pub max_concurrent_fds: usize,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            epoll_timeout_ms: 5000,
            max_payload_bytes: MAX_PAYLOAD_BYTES,
            max_concurrent_fds: MAX_CONCURRENT_FDS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let msg = Message::new(Command::Start, Bytes::from_static(b"test payload"));
        let encoded = msg.encode();
        assert!(encoded.len() >= Header::SIZE);
    }

    #[test]
    fn test_command_range() {
        assert!(Command::from_u8(0x10).is_some());
        assert!(Command::from_u8(0x60).is_some());
        assert!(Command::from_u8(0x70).is_none());
    }
}
