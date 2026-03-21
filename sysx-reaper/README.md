# sysx-reaper

**cgroup.events epoll monitor + sweep coordination hooks**

Implements the epoll side of the SysX stop ladder (per `12-canonical-spec-v1.md` §4.1.1 and section 04).

## Purpose

- Monitor `cgroup.events` files via epoll for `populated=0` transitions
- Coordinate sweep after stop ladder timeout (5s → Tombstoned)
- Execute mandatory **DFS post-order rmdir** of cgroup subtree to prevent EBUSY
- Provide hooks for integration with sysxd PID 1

## Key Contracts

- Kernel cgroup state is authoritative truth (epistemic severance)
- 5000ms hard timeout on populated=0 wait
- Post-order unlink required for Dead state
- Clean integration with sweep coordination from core

## Build

```bash
cargo build --release
```

## Structure

- `src/lib.rs` - Core Reaper logic and cgroup monitoring
- `src/main.rs` - CLI entrypoint
- `Cargo.toml` - Dependencies (tokio, nix, etc.)

Part of the SysX deterministic PID 1 system.
