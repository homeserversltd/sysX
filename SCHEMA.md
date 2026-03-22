# Service schema (YAML) — field reference

Sidecar to **`README.md`**. This lists every key **`sysxd`** / **`sysx-schema`** can deserialize into **`ServiceSchema`** (`sysx-schema/src/lib.rs`). Canonical prose contract: **`docs/memories/ongoing/sysX/15-schema-contract-v1.md`** in the main repo (when this tree is submoduled).

**Load path:** `/etc/sysx/schemas/*.yaml` (or `*.yml`), or **`SYSX_SCHEMA_DIR`**.

**Validation today:** `sysx_version == 1`, non-empty **`service.name`** matching `^[a-z0-9_-]{1,32}$`. Unknown YAML keys may still be accepted by the parser until **`from_yaml_strict`** is implemented; do not rely on silent rejection.

---

## Top-level keys

| Key | Required | Type | Notes |
|-----|----------|------|--------|
| `sysx_version` | yes | integer | Must be **`1`**. |
| `enabled` | no | bool | Default **`false`**. If false or omitted, service stays **Offline** (no DAG spawn). |
| `service` | yes | map | See **§ `service`**. |
| `type` | yes | string | **`simple`** or **`fdpipe`** (serde lowercase enum names). |
| `timeout_sec` | no | integer | `u16`. Intended for **`fdpipe`** readiness on FD 3; see doctrine **`15`** §2. |
| `depends_on` | no | list of strings | Default **`[]`**. Service names that must be **Running** before this one starts (DAG). |
| `recovery` | no | map | See **§ `recovery`**. Omit if no auto-restart policy. |
| `pids` | no | map | See **§ `pids`**. |
| `memory` | no | map | See **§ `memory`**. |
| `cpu` | no | map | See **§ `cpu`**. |
| `namespaces` | no | map | See **§ `namespaces`**. |
| `capabilities` | no | map | See **§ `capabilities`**. |
| `rootfs` | no | map | See **§ `rootfs`**. |
| `logging` | no | map | See **§ `logging`**. |

---

## `service` (required)

| Key | Required | Type | Notes |
|-----|----------|------|--------|
| `name` | yes | string | **`^[a-z0-9_-]{1,32}$`**. cgroup: `/sys/fs/cgroup/<slice>/<name>/`. |
| `exec` | yes | string | Absolute path to binary for **`execve`**. |
| `args` | yes | list of strings | argv after exec (excluding argv0; runtime sets argv0 from exec). |
| `env` | yes | map string → string | Child environment; empty `{}` allowed. |
| `user` | yes | string | Numeric UID string (e.g. `"0"`, `"1000"`) for privilege drop path in **`sysxd`**. |

---

## `type` (required)

| Value | Meaning |
|-------|---------|
| `simple` | Declared type for the common **`spawn_simple_service`** path (readiness via FD 3 in current **`sysxd`**). |
| `fdpipe` | Second enum variant (`FdPipe`); reserved for FD-oriented contracts per **`15`** §2. Parse-time valid; ensure runtime/compiler match your policy. |

---

## `recovery` (optional)

All keys required if **`recovery`** is present.

| Key | Type | Notes |
|-----|------|--------|
| `policy` | string | **`always`**, **`on_failure`**, or **`manual`** (lowercase). |
| `backoff_ms` | integer | `u64`. Base for exponential backoff. |
| `max_restarts` | integer | `u32`. Circuit breaker count in rolling window. |
| `window_sec` | integer | `u64`. Sliding window for restart accounting. |

---

## `pids` (optional)

| Key | Type | Notes |
|-----|------|--------|
| `max` | integer | `u32`. Written to cgroup **`pids.max`**. |

---

## `memory` (optional)

Both keys required if **`memory`** is present.

| Key | Type | Notes |
|-----|------|--------|
| `max_mb` | integer | `u32`. Converted to bytes, **`memory.max`**. |
| `swap_mb` | integer | `u32`. Converted to bytes, **`memory.swap.max`**. |

---

## `cpu` (optional)

| Key | Type | Notes |
|-----|------|--------|
| `weight` | integer | `u16`. Doctrine range **1–10000**, written to **`cpu.weight`**. |

---

## `namespaces` (optional)

If the **`namespaces`** map is present, supply at least **`mount`**, **`network`**, **`pid`**, **`user`** (bools). **`uts`** may be omitted and defaults to **`false`**.

| Key | Type | Notes |
|-----|------|--------|
| `mount` | bool | **`CLONE_NEWNS`** when true. |
| `network` | bool | **`CLONE_NEWNET`** when true. |
| `pid` | bool | **`CLONE_NEWPID`** (full support may be deferred; see **`service_spawn`** docs). |
| `uts` | bool | Optional. **`CLONE_NEWUTS`**. Defaults to **`false`**. |
| `user` | bool | **`CLONE_NEWUSER`**. |

---

## `capabilities` (optional)

| Key | Type | Notes |
|-----|------|--------|
| `keep` | list of strings | Capability names (e.g. **`CAP_NET_BIND_SERVICE`**) or numeric indices; see **`service_spawn`** mapping. |

---

## `rootfs` (optional)

| Key | Type | Notes |
|-----|------|--------|
| `read_only` | bool | Remount **`/`** read-only when mount namespace applies. |
| `tmpfs` | map or null | Path → tmpfs mount options string (e.g. **`size=64M,mode=1777`**). Omitted or null if unused. |

---

## `logging` (optional)

| Key | Required | Type | Notes |
|-----|----------|------|--------|
| `stdout` | yes | string | Absolute path; opened, **`dup2`** to FD 1. |
| `stderr` | yes | string | Absolute path; **`dup2`** to FD 2. |
| `log_rotate_signal` | no | string | Optional; doctrine suggests enum validation (e.g. **`SIGHUP`**). |

---

## Minimal valid example

```yaml
sysx_version: 1
enabled: true
service:
  name: example
  exec: /bin/true
  args: []
  env: {}
  user: "0"
type: simple
depends_on: []
```

---

## See also

- **`sysx-compiler`** `compile` subcommand for static validation at forge time.
- Wire and status bytes: **`12-canonical-spec-v1.md`**. DAG and depth: **`16-sysx-recovery-dag-bootstrap-and-failure-reasons.md`**.
