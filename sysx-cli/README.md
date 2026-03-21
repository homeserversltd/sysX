# sysx-cli

Command line interface for SysX cgroup supervisor.

Speaks the binary IPC wire format from 12-canonical-spec-v1.md §2 to /run/sysx/control.sock.

Maps outcome bytes (§2.1) vs Status ABI (§2.1.1) correctly based on command sent.

Usage:
  sysx start <service>
  sysx stop <service>
  sysx status <service>
  sysx poweroff
  sysx reboot

See docs/memories/ongoing/sysX/ for wire protocol details.
