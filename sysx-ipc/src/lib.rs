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

/// Command opcodes — **`12-canonical-spec-v1.md` §2** (v1.4 wire; no other values on the wire).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    /// Start (0x10) — UTF-8 service name in payload
    Start = 0x10,
    /// Stop (0x20)
    Stop = 0x20,
    /// Status (0x30)
    Status = 0x30,
    /// Reset (0x40)
    Reset = 0x40,
    /// Poweroff / Terminal Broadcast (0x50) — empty payload
    Poweroff = 0x50,
    /// Reboot (0x60) — empty payload
    Reboot = 0x60,
}

impl Command {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x10 => Some(Command::Start),
            0x20 => Some(Command::Stop),
            0x30 => Some(Command::Status),
            0x40 => Some(Command::Reset),
            0x50 => Some(Command::Poweroff),
            0x60 => Some(Command::Reboot),
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
        let pl = (self.payload_length as usize)
            .min(MAX_PAYLOAD_BYTES)
            .min(u16::MAX as usize) as u16;
        buf.put_u16_le(pl);
        buf.put_u64(0); // §2: 8 reserved bytes
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

        let payload_length = buf.get_u16_le() as u32;
        let _ = buf.get_u64(); // 8 reserved

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

/// §2.1 outcome: `status_code` = `0x00` success, `0x02` unauthorized (`SO_PEERCRED`).
pub const OUTCOME_SUCCESS: u8 = 0x00;
pub const OUTCOME_UNAUTHORIZED: u8 = 0x02;
pub const REASON_NA: u8 = 0x00;

/// §2.1.1 Status reply: primary state `Running`, no secondary reason.
pub const STATUS_RUNNING: u8 = 0x02;
pub const STATUS_REASON_NONE: u8 = 0x00;

/// SYSX frame for a response: same header shape as requests, **2-byte** body (`§2.1` / `§2.1.1`).
#[must_use]
pub fn encode_ipc_reply(request_command: Command, byte0: u8, byte1: u8) -> Bytes {
    Message::new(
        request_command,
        Bytes::copy_from_slice(&[byte0, byte1]),
    )
    .encode()
}

/// Decode header + payload slice from a contiguous buffer (single datagram read).
/// Fails if the buffer is shorter than `Header::SIZE + header.payload_length`.
pub fn decode_frame(buf: &[u8]) -> Result<(Header, &[u8]), FrameError> {
    if buf.len() < Header::SIZE {
        return Err(FrameError::TruncatedHeader);
    }
    let header = Header::decode(Bytes::copy_from_slice(&buf[..Header::SIZE]))?;
    let plen = header.payload_length as usize;
    let total = Header::SIZE.saturating_add(plen);
    if buf.len() < total {
        return Err(FrameError::TruncatedHeader);
    }
    Ok((header, &buf[Header::SIZE..total]))
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
        let (h, pl) = decode_frame(&encoded).expect("decode_frame");
        assert_eq!(h.command, Command::Start);
        assert_eq!(pl, b"test payload");
    }

    #[test]
    fn test_command_range() {
        assert!(Command::from_u8(0x10).is_some());
        assert_eq!(Some(Command::Stop), Command::from_u8(0x20));
        assert_eq!(Some(Command::Status), Command::from_u8(0x30));
        assert!(Command::from_u8(0x60).is_some());
        assert!(Command::from_u8(0x11).is_none());
        assert!(Command::from_u8(0x70).is_none());
    }

    #[test]
    fn test_ipc_reply_roundtrip() {
        let b = encode_ipc_reply(Command::Status, 0x02, 0x00);
        let (h, pl) = decode_frame(&b).expect("reply decode");
        assert_eq!(h.command, Command::Status);
        assert_eq!(pl, &[0x02, 0x00]);
    }
}
