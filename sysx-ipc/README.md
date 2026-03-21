# sysx-ipc

SysX IPC v1 wire protocol and shared types.

Implements the binary framing, command space (0x10-0x60), SO_PEERCRED contract, and bounds from the canonical spec (12-canonical-spec-v1.md §2).

## Features

- Fixed 16-byte header: `SYSX`, version, **§2 command**, **`u16` payload length LE**, 8 reserved bytes
- Command enum: **`0x10` Start, `0x20` Stop, `0x30` Status, `0x40` Reset, `0x50` Poweroff, `0x60` Reboot** only
- Strict payload size enforcement (4096 bytes max)
- SO_PEERCRED peer validation helpers
- CoreConfig carrying `epoll_timeout_ms`

## Usage

```rust
use sysx_ipc::{Command, Message, Header, validate_peer_credentials};

// Create a message
let msg = Message::new(Command::Start, bytes::Bytes::from_static(b"payload"));

// Encode to wire format
let frame = msg.encode();
```

## Constraints Enforced

- `MAX_PAYLOAD_BYTES`: 4096
- `MAX_CONCURRENT_FDS`: 64
- `epoll_timeout_ms` pulled from core config
- SO_PEERCRED validation on control socket accept
- Clean error types with `thiserror`

This crate is **only** for other sysX crates.

