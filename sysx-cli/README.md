# sysx-cli

Binary client for **`12-canonical-spec-v1.md` §2** (same framing as `sysx-ipc`).

Build: `cargo build -p sysx-cli --bin sysx` → `target/debug/sysx`.

```text
sysx --socket /run/sysx/control.sock status <service>
sysx start <service> | stop <service> | reset <service>
sysx poweroff | reboot
```

`SYSX_SOCKET` overrides the default path. Requires **`sysxd`** listening on that socket (PID 1 or dev run).
