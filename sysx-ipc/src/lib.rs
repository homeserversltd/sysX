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

// --- Sealed `/etc/sysx/core.bin` (17-sysx-sealed-boot-core-bin.md §4, 12 §2.2) ---

/// Fixed file size for `SysxCoreConfig` on disk.
pub const CORE_BIN_SIZE: usize = 32;

/// Sealed core magic (`"SYSX"`).
pub const CORE_BIN_MAGIC: &[u8; 4] = b"SYSX";

/// `core.bin` format version (must match forge and PID 1 decode).
pub const CORE_BIN_VERSION: u8 = 1;

/// Runtime policy from sealed `core.bin` (numeric fields only; no heap parse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysxCoreBin {
    pub admin_gid: u32,
    pub max_cgroups: u32,
    pub epoll_timeout_ms: u16,
}

/// `core.bin` validation / decode errors (PID 1 must fail closed).
#[derive(Error, Debug, PartialEq, Eq)]
pub enum CoreBinError {
    #[error("core.bin: expected {CORE_BIN_SIZE} bytes, got {0}")]
    BadSize(usize),
    #[error("core.bin: bad magic (expected SYSX)")]
    BadMagic,
    #[error("core.bin: unsupported version {0}")]
    BadVersion(u8),
}

/// Decode 32-byte sealed artifact from disk (`17` §4, little-endian multi-byte fields).
pub fn decode_core_bin(data: &[u8]) -> Result<SysxCoreBin, CoreBinError> {
    if data.len() != CORE_BIN_SIZE {
        return Err(CoreBinError::BadSize(data.len()));
    }
    if data[0..4] != CORE_BIN_MAGIC[..] {
        return Err(CoreBinError::BadMagic);
    }
    if data[4] != CORE_BIN_VERSION {
        return Err(CoreBinError::BadVersion(data[4]));
    }
    Ok(SysxCoreBin {
        admin_gid: u32::from_le_bytes([data[5], data[6], data[7], data[8]]),
        max_cgroups: u32::from_le_bytes([data[9], data[10], data[11], data[12]]),
        epoll_timeout_ms: u16::from_le_bytes([data[13], data[14]]),
    })
}

/// Encode canonical 32-byte `core.bin` (forge-time / tests). Reserved tail is zeroed.
pub fn encode_core_bin(admin_gid: u32, max_cgroups: u32, epoll_timeout_ms: u16) -> [u8; CORE_BIN_SIZE] {
    let mut b = [0u8; CORE_BIN_SIZE];
    b[0..4].copy_from_slice(CORE_BIN_MAGIC);
    b[4] = CORE_BIN_VERSION;
    b[5..9].copy_from_slice(&admin_gid.to_le_bytes());
    b[9..13].copy_from_slice(&max_cgroups.to_le_bytes());
    b[13..15].copy_from_slice(&epoll_timeout_ms.to_le_bytes());
    b
}

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
/// §2.1 **`0x03`** — wire-level parse / pre-dispatch frame rejection (`12` §2.1).
pub const OUTCOME_WIRE_PARSE: u8 = 0x03;
/// §2.1 **`0x04`** — epistemic conflict / physical denial (e.g. **`Start`** blocked by **`max_cgroups`**, `12` §2.2).
pub const OUTCOME_EPISTEMIC_CONFLICT: u8 = 0x04;
/// §2.1 **`0x05`** — **Tombstoned** outcome (e.g. **`Stop`** ladder step 5 timeout, `12` §4.1, `16` §4).
pub const OUTCOME_TOMBSTONED: u8 = 0x05;
pub const REASON_NA: u8 = 0x00;
/// §2.1 **`reason_code`** **`0x06`** — cgroup capacity (**`max_cgroups`** from **`core.bin`**) with **`status_code`** **`0x04`** (`12` §2.2).
pub const REASON_CGROUP_CAPACITY: u8 = 0x06;

/// §2.1.1 Status primary states (`byte0`). **Only** `Command::Status` uses this table (`12` §2.1.1).
/// Note: **`0x05` / `0x06` here are lifecycle states**, not `§2.1` outcome `Tombstoned` (`0x05` on Stop).
pub const STATUS_OFFLINE: u8 = 0x00;
/// §2.1.1 Status reply: primary state `Running`, no secondary reason.
pub const STATUS_RUNNING: u8 = 0x02;
/// §2.1.1 — cgroup exists, `populated=0`, directory not yet DFS-unlinked (`12` §3).
pub const STATUS_SWEEPING: u8 = 0x03;
/// §2.1.1 — **`Failed`** primary state (`12` §2.1.1); byte1 = `FailureReason` (e.g. **`0x01`** Orphaned).
pub const STATUS_FAILED_PRIMARY: u8 = 0x05;
/// §2.1.1 — **`Tombstoned`** lifecycle state on **`Status`** (`12` §2.1.1, `16` §4); pair **`[0x06, 0x00]`**.
pub const STATUS_TOMBSTONED_PRIMARY: u8 = 0x06;
pub const STATUS_REASON_NONE: u8 = 0x00;
/// §2.1.1 — **`Failed`** + **`Orphaned`** (`11` dual-oracle / `12` §2.1.1 reason column).
pub const STATUS_REASON_ORPHANED: u8 = 0x01;

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

/// SO_PEERCRED read (before trusting bytes). Pair with [`peer_allowed_control`].
pub fn validate_peer_credentials(fd: RawFd) -> Result<UnixCredentials, FrameError> {
    // SO_PEERCRED via getsockopt (nix 0.29 expects AsFd — use BorrowedFd::borrow_raw)
    unsafe {
        let borrowed = BorrowedFd::borrow_raw(fd);
        nix::sys::socket::getsockopt(&borrowed, nix::sys::socket::sockopt::PeerCredentials)
            .map_err(|_| FrameError::CredValidationFailed)
    }
}

/// Operator policy (`14` §4, `17` §5): **`uid == 0`** or **`gid == admin_gid`** from sealed `core.bin`.
pub fn peer_allowed_control(creds: &UnixCredentials, admin_gid: u32) -> bool {
    creds.uid() == 0 || creds.gid() as u32 == admin_gid
}

/// Spec constants not stored in `core.bin` (12 §2.2); kept for helpers / docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    pub max_payload_bytes: usize,
    pub max_concurrent_fds: usize,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
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

    #[test]
    fn core_bin_roundtrip_17() {
        let raw = encode_core_bin(1000, 256, 100);
        let c = decode_core_bin(&raw).expect("decode");
        assert_eq!(c.admin_gid, 1000);
        assert_eq!(c.max_cgroups, 256);
        assert_eq!(c.epoll_timeout_ms, 100);
        assert_eq!(raw.len(), CORE_BIN_SIZE);
    }

    #[test]
    fn core_bin_rejects_wrong_magic() {
        let mut raw = encode_core_bin(1, 2, 3);
        raw[0] = b'X';
        assert_eq!(decode_core_bin(&raw), Err(super::CoreBinError::BadMagic));
    }
}
