# sysX

**Pre-release — not production-ready.**

SysX is a Rust control stack for the machine: it supervises services and, more importantly, **spawns every workload into a policy-defined environment** you declare in schema. Namespaces (mount, network, PID, user), capability sets, cgroup envelopes (CPU weight, memory, PIDs), and filesystem view (read-only root, tmpfs maps) are first-class inputs, not afterthoughts bolted on outside the supervisor.

That means you can land a process in a **minimal cage** (for example: no usable network view, no inherited file hierarchy you did not explicitly mount, privilege dropped before `execve`) or run a conventional service under **hard resource bounds** (CPU share, RAM, process count) with the same contract. Lifecycle, cleanup, and observability stay tied to **cgroup v2 ground truth**, not PID files or wrapper scripts.

**What you get:** an open execution fabric—declarative service definitions, a compiler path to sealed boot artifacts, and a small binary control surface so operators can reason about *what runs where and under what limits* from data, not folklore.

## Repository layout

| Path | Role |
|------|------|
| `sysx-schema/` | YAML → `ServiceSchema` with strict validation |
| `sysx-compiler/` | Compiler CLI and sealed `core.bin` forger |
| `sysx-runtime/` | Cgroup / namespace / capability policy types and path helpers |
| `sysxd/` | Supervisor (reactor, lifecycle) |
| `sysx-cli/` | Operator CLI |
| `sysx-ipc/` | Binary IPC framing |

## Build

From the repository root:

```bash
cargo build
```

## Quick usage

```bash
# Compile a service schema
cargo run -p sysx-compiler -- compile /etc/sysx/schemas/example.yaml

# Forge sealed core.bin (32-byte PID 1 boot artifact)
cargo run -p sysx-compiler -- forge-core /etc/sysx/core.bin 1000
```

## License

See `LICENSE`.
