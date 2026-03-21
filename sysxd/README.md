# sysxd

PID 1 replacement: initramfs handoff, boot laser (watchdog → `core.bin` → control socket), Kahn DAG pre-reactor validation, main epoll reactor.

## Entry

- `src/main.rs` — process entry
- `src/lib.rs` — `boot`, `dag`, `reactor` modules

## Config

Root workspace `index.json` and `/etc/sysx/` on the target system per `13-sysx-rust-infinite-index-layout.md`.
