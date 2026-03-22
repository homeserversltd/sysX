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
cargo test
```

## Testing status and adoption frontier

**Automated testing (repo gate):** The phased implementation (supervisor, schemas, DAG orchestration, stop ladder, terminal broadcast, recovery/circuit behavior, reaper path, and the `tools/install.py` initramfs pack plus optional QEMU checks) is exercised with **`cargo test`** at the workspace root and with the install tool where a kernel is available. Treat that as **“software proof in-tree,”** not a warranty on every workload or host.

**Next step for anyone picking this up: bare metal (or a serious VM):** Running `sysxd` as PID 1 on real hardware means supplying a kernel, an initramfs that matches what `install.py` forges (`/init`, `/etc/sysx/core.bin`, `/etc/sysx/schemas/`, cgroup v2), and bootloader configuration. That path is **intentionally** the next frontier: automated runs cannot substitute for firmware, drivers, and consoles on physical boxes.

**What is still “your problem” outside the automated harness:** The supervisor owns **declared services and cgroup truth** per schema. It does **not** replace a full general-purpose init ecosystem on its own. Expect to own or integrate **network bring-up** (interfaces, routing, DNS client policy), **getty / login** if you want a console, **disk and fstab** if you pivot root or mount more than the initramfs, **clock and time**, and **recovery** (second boot entry, serial console, or initramfs shell) if PID 1 misbehaves. Those concerns are normal for any replacement init; they are called out here so adopters do not assume QEMU parity on first metal boot.

## Quick usage

```bash
# Compile a service schema
cargo run -p sysx-compiler -- compile /etc/sysx/schemas/example.yaml

# Forge sealed core.bin (32-byte PID 1 boot artifact)
cargo run -p sysx-compiler -- forge-core /etc/sysx/core.bin 1000
```

## License

See `LICENSE`.
