# sysx-runtime

Band: lifecycle, cgroup v2 paths, namespace/capability policy (fork/exec choreography), and coordination with `sysx-reaper`.

## Invariants

- Service IDs are single path segments; no `/` or `..` (see `validate_service_id`).
- Cgroup root defaults to `/sys/fs/cgroup`; override via `SYSX_CGROUP_ROOT`.

## Orchestrator

Rust library `src/lib.rs` (no Python `index.py`). Parent: workspace root `index.json`.

See `docs/memories/ongoing/sysX/13-sysx-rust-infinite-index-layout.md`.
