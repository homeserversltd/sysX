# sysX
*TESTING DO NOT USE STILL BUILDING*
Deterministic PID 1 for sovereign infrastructure.

## Structure

- `sysx-schema/` — YAML → `ServiceSchema` with strict unknown field validation (per `15-schema-contract-v1.md`)
- `sysx-compiler/` — Compiler CLI + core.bin forger (`17-sysx-sealed-boot-core-bin.md` §4)

## Building

```bash
cd /home/owner/git/sysX
cargo build
```

## Usage

```bash
# Compile a service schema
cargo run --bin sysx-compiler -- compile /etc/sysx/schemas/example.yaml

# Forge sealed core.bin (32-byte artifact for PID 1)
cargo run --bin sysx-compiler -- forge-core /etc/sysx/core.bin 1000
```

## Contracts

- **Schema**: `15-schema-contract-v1.md` — absolute source of truth. Unmapped fields = hard fault.
- **Core Binary**: `17-sysx-sealed-boot-core-bin.md` — 32-byte fixed layout, zero-allocation read in PID 1.
- **Canonical Spec**: `12-canonical-spec-v1.md`

See `docs/memories/ongoing/sysX/` for full specification.

This implementation satisfies the batch directive for AMAYMON_WORKER 3/9 track_label=3.
