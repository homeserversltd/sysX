# sysX

**Pre-release — not production-ready.**

SysX is a Rust control stack for the machine: it supervises services and, more importantly, **spawns every workload into a policy-defined environment** you declare in schema. Namespaces (mount, network, PID, user), capability sets, cgroup envelopes (CPU weight, memory, PIDs), and filesystem view (read-only root, tmpfs maps) are first-class inputs, not afterthoughts bolted on outside the supervisor.

That means you can land a process in a **minimal cage** (for example: no usable network view, no inherited file hierarchy you did not explicitly mount, privilege dropped before `execve`) or run a conventional service under **hard resource bounds** (CPU share, RAM, process count) with the same contract. Lifecycle, cleanup, and observability stay tied to **cgroup v2 ground truth**, not PID files or wrapper scripts.

**What you get:** an open execution fabric—declarative service definitions, a compiler path to sealed boot artifacts, and a small binary control surface so operators can reason about *what runs where and under what limits* from data, not folklore.

## Repository layout

| Path | Role |
|------|------|
| `SCHEMA.md` | Every YAML key accepted by **`ServiceSchema`** (sidecar reference) |
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

## Service schemas (on-disk layout)

PID 1 loads **every** `*.yaml` / `*.yml` in this directory:

**`/etc/sysx/schemas/`**

Override the path with env **`SYSX_SCHEMA_DIR`** (tests, custom roots). Each file is one `ServiceSchema`; `service.name` must be unique and match `^[a-z0-9_-]{1,32}$`. `type` is `simple` or `fdpipe` (see `15-schema-contract-v1.md`). **Full key list:** **`SCHEMA.md`** (next to this file).

Below are five illustrative shapes you might deploy (adjust `exec`, paths, and limits to your rootfs).

### 1. `minimal.yaml` — smallest boot-safe unit

```yaml
sysx_version: 1
enabled: true
service:
  name: minimal
  exec: /bin/sh
  args: ["-c", "echo minimal_ok; sleep infinity"]
  env: {}
  user: "0"
type: simple
depends_on: []
```

### 2. `edge_logger.yaml` — file logging targets

```yaml
sysx_version: 1
enabled: true
service:
  name: edge_logger
  exec: /usr/bin/logger_helper
  args: ["--foreground"]
  env: {}
  user: "65534"
type: simple
depends_on: []
logging:
  stdout: /var/log/sysx/edge_logger.out
  stderr: /var/log/sysx/edge_logger.err
```

### 3. `api.yaml` — DAG edge (`depends_on`)

`api` starts only after **`registry`** is already running (Kahn order). Add a separate `registry.yaml` with `service.name: registry` in the same directory.

```yaml
sysx_version: 1
enabled: true
service:
  name: api
  exec: /opt/app/bin/api_server
  args: ["--listen", ":8443"]
  env:
    RUST_LOG: info
  user: "1000"
type: simple
depends_on: [registry]
```

### 4. `worker.yaml` — cgroup envelope + optional namespaces

```yaml
sysx_version: 1
enabled: true
service:
  name: worker
  exec: /opt/jobs/runner
  args: []
  env: {}
  user: "1000"
type: simple
depends_on: []
cpu:
  weight: 200
memory:
  max_mb: 512
  swap_mb: 0
pids:
  max: 256
namespaces:
  mount: true
  network: false
  pid: false
  uts: false
  user: false
capabilities:
  keep: ["CAP_NET_BIND_SERVICE"]
```

### 5. `sync_agent.yaml` — recovery policy (restart budget)

```yaml
sysx_version: 1
enabled: true
service:
  name: sync_agent
  exec: /usr/lib/sysx/sync_agent
  args: ["--once"]
  env: {}
  user: "0"
type: simple
depends_on: []
recovery:
  policy: on_failure
  backoff_ms: 500
  max_restarts: 5
  window_sec: 120
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
