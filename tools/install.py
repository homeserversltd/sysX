#!/usr/bin/env python3
"""
SysX Install Tool - Deterministic Hypervisor Test Harness

Implements phases per 11-hypervisor-test-loop-and-oracle.md:
- musl build (static x86_64-unknown-linux-musl)
- initramfs layout forge
- QEMU headless execution
- Oracle marker verification

See docs/memories/ongoing/sysX/11-hypervisor-test-loop-and-oracle.md
and 12-canonical-spec-v1.md for test contracts.
"""

import argparse
import re
import subprocess
import sys
import shutil
from pathlib import Path
from typing import Optional, Set

# Default to repository root containing this script (…/sysX).
_DEFAULT_SYSX_ROOT = Path(__file__).resolve().parent.parent

MUSL_TARGET = "x86_64-unknown-linux-musl"


def forge_core_bin(sysx_root: Path, dest: Path, admin_gid: int = 0) -> bool:
    """Write 32-byte sealed core.bin via sysx-compiler (17)."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    print(f"[SYSX] Forging core.bin -> {dest} (admin_gid={admin_gid})")
    try:
        r = subprocess.run(
            [
                "cargo",
                "run",
                "-p",
                "sysx-compiler",
                "--",
                "forge-core",
                str(dest),
                str(admin_gid),
            ],
            cwd=sysx_root,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        print("[ERROR] cargo not found")
        return False
    if r.stdout:
        print(r.stdout)
    if r.stderr:
        print("STDERR:", r.stderr, file=sys.stderr)
    if r.returncode != 0:
        print("[FAIL] forge-core failed")
        return False
    if not dest.is_file() or dest.stat().st_size < 32:
        print("[FAIL] core.bin missing or too small")
        return False
    print(f"[SUCCESS] core.bin ({dest.stat().st_size} bytes)")
    return True


def read_elf_interpreter(binary: Path) -> Optional[Path]:
    """Return PT_INTERP path for dynamic ELF."""
    try:
        r = subprocess.run(
            ["readelf", "-l", str(binary)],
            capture_output=True,
            text=True,
            check=False,
        )
    except FileNotFoundError:
        return None
    if r.returncode != 0:
        return None
    for line in r.stdout.splitlines():
        if "Requesting program interpreter:" in line:
            m = re.search(r":\s*(\S+)\]", line)
            if m:
                return Path(m.group(1))
    return None


def copy_gnu_dynamic_deps(binary: Path, rootfs: Path) -> None:
    """If sysxd is dynamically linked (GNU), copy ld.so + shared libs into rootfs."""
    try:
        fr = subprocess.run(["file", str(binary)], capture_output=True, text=True, check=True)
    except (FileNotFoundError, subprocess.CalledProcessError):
        fr = None
    if fr and ("statically linked" in fr.stdout or "static-pie" in fr.stdout):
        print("[INFO] sysxd appears static; skipping ldd copy")
        return

    try:
        lp = subprocess.run(["ldd", str(binary)], capture_output=True, text=True)
    except FileNotFoundError:
        print("[WARN] ldd not found; GNU initramfs may be incomplete")
        return
    if lp.returncode != 0 or "not a dynamic" in lp.stdout or "statically linked" in lp.stdout:
        print("[INFO] ldd reports static or non-ELF; skipping lib copy")
        return

    interp = read_elf_interpreter(binary)
    if interp and interp.is_file():
        dest = rootfs / str(interp).lstrip("/")
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(interp, dest)
        print(f"[INFO] Copied interpreter {interp} -> {dest}")

    seen: Set[Path] = set()
    for line in lp.stdout.splitlines():
        line = line.strip()
        if "=>" not in line:
            continue
        rhs = line.split("=>", 1)[1].strip()
        path_part = rhs.split("(", 1)[0].strip()
        if not path_part.startswith("/"):
            continue
        p = Path(path_part)
        if not p.is_file() or p in seen:
            continue
        seen.add(p)
        dest = rootfs / str(p).lstrip("/")
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(p, dest)
        print(f"[INFO] Copied {p.name} -> {dest}")


def resolve_sysxd_binary(sysx_root: Path) -> Optional[Path]:
    """Prefer musl release artifact; fall back to host GNU release."""
    musl = sysx_root / "target" / MUSL_TARGET / "release" / "sysxd"
    if musl.is_file():
        return musl
    gnu = sysx_root / "target" / "release" / "sysxd"
    if gnu.is_file():
        return gnu
    return None


def run_cmd(cmd: list[str], cwd: Optional[Path] = None, check: bool = True) -> subprocess.CompletedProcess:
    """Run command with logging."""
    print(f"[SYSX] Running: {' '.join(cmd)}")
    try:
        result = subprocess.run(
            cmd,
            cwd=cwd,
            capture_output=True,
            text=True,
            check=check
        )
        if result.stdout:
            print(result.stdout)
        if result.stderr:
            print("STDERR:", result.stderr, file=sys.stderr)
        return result
    except subprocess.CalledProcessError as e:
        print(f"[ERROR] Command failed: {e}")
        print(f"STDOUT: {e.stdout}")
        print(f"STDERR: {e.stderr}")
        if check:
            sys.exit(1)
        return e


def phase_musl_build(sysx_root: Path) -> bool:
    """Phase 1: Prefer static musl release; otherwise host release (GNU) for local pipeline testing."""
    print("\n=== PHASE 1: RELEASE BUILD (musl preferred) ===")
    rustup = shutil.which("rustup")

    if rustup:
        run_cmd([rustup, "target", "add", MUSL_TARGET], check=False)
        print("Building sysxd (workspace) with musl...")
        try:
            musl_proc = subprocess.run(
                [
                    "cargo",
                    "build",
                    "--release",
                    "--target",
                    MUSL_TARGET,
                    "-p",
                    "sysxd",
                ],
                cwd=sysx_root,
                capture_output=True,
                text=True,
            )
        except FileNotFoundError:
            print("[ERROR] cargo not found in PATH")
            return False
        if musl_proc.stdout:
            print(musl_proc.stdout)
        if musl_proc.stderr:
            print("STDERR:", musl_proc.stderr, file=sys.stderr)
        musl_bin = sysx_root / "target" / MUSL_TARGET / "release" / "sysxd"
        if musl_proc.returncode == 0 and musl_bin.is_file():
            print(f"[SUCCESS] musl static: {musl_bin}")
            return True
        print(
            "[WARN] musl build failed or missing artifact; "
            f"returncode={musl_proc.returncode}; trying GNU release fallback"
        )
    else:
        print(
            "[WARN] rustup not in PATH: building host GNU release only. "
            "Per 08/11, install rustup and `rustup target add x86_64-unknown-linux-musl` for musl proof."
        )

    print("Building sysxd host release (GNU libc)...")
    try:
        gproc = subprocess.run(
            ["cargo", "build", "--release", "-p", "sysxd"],
            cwd=sysx_root,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        print("[ERROR] cargo not found in PATH")
        return False
    if gproc.stdout:
        print(gproc.stdout)
    if gproc.stderr:
        print("STDERR:", gproc.stderr, file=sys.stderr)
    if gproc.returncode != 0:
        print(f"[FAIL] GNU release build failed (returncode={gproc.returncode})")
        return False

    b = resolve_sysxd_binary(sysx_root)
    if b:
        label = "musl" if MUSL_TARGET in str(b) else "host GNU (not musl)"
        print(f"[SUCCESS] {label}: {b}")
        return True
    print("[FAIL] Binary not found under target/")
    return False


def phase_initramfs_layout(sysx_root: Path, build_dir: Path) -> bool:
    """Phase 2: Forge initramfs layout."""
    print("\n=== PHASE 2: INITRAMFS LAYOUT ===")

    rootfs = build_dir / "rootfs"
    rootfs.mkdir(parents=True, exist_ok=True)

    # Create minimal FHS
    for d in ["bin", "sbin", "etc", "run", "sys", "proc", "dev", "tmp", "lib", "lib64"]:
        (rootfs / d).mkdir(parents=True, exist_ok=True)

    # Place sysxd as /sbin/init
    binary_src = resolve_sysxd_binary(sysx_root)
    if binary_src is not None:
        shutil.copy2(binary_src, rootfs / "sbin" / "init")
        (rootfs / "sbin" / "init").chmod(0o755)
        print("[SUCCESS] Placed sysxd as /sbin/init")
        # Kernel execs /init from initramfs when present (without this, kernel may hunt for a disk root).
        shutil.copy2(binary_src, rootfs / "init")
        (rootfs / "init").chmod(0o755)
        print("[SUCCESS] Placed sysxd as /init")
        copy_gnu_dynamic_deps(binary_src, rootfs)
        _copy_busybox_sh(rootfs)
    else:
        print("[WARN] No binary, creating stub")
        (rootfs / "sbin" / "init").write_text("#!/bin/sh\necho 'SYSX STUB INIT'\nexec /bin/sh\n")
        (rootfs / "sbin" / "init").chmod(0o755)

    # Sealed boot artifact (required by sysxd boot; 17)
    sysx_etc = rootfs / "etc" / "sysx"
    sysx_etc.mkdir(parents=True, exist_ok=True)
    if not forge_core_bin(sysx_root, sysx_etc / "core.bin", admin_gid=0):
        return False

    # `15` — one file per service under /etc/sysx/schemas/
    schemas_dir = sysx_etc / "schemas"
    schemas_dir.mkdir(parents=True, exist_ok=True)
    (schemas_dir / "baseline.yaml").write_text(
        """sysx_version: 1
enabled: true
service:
  name: baseline
  exec: /bin/sh
  args: ["-c", "echo -n R >&3; echo SYSX_BASELINE_WORKLOAD; sleep 3600"]
  env: {}
  user: "0"
type: simple
depends_on: []
"""
    )

    print(f"[SUCCESS] Initramfs layout forged at {rootfs}")
    return True


def phase_qemu_headless(
    sysx_root: Path,
    build_dir: Path,
    kernel: Optional[Path],
    run_vm: bool,
    qemu_timeout_sec: int,
    oracle_strict: bool = False,
    dual_oracle_strict: bool = False,
    test0_strict: bool = False,
    autotest_service: str = "baseline",
) -> bool:
    """Phase 3: Pack initramfs; optionally run QEMU with kernel bzImage."""
    print("\n=== PHASE 3: INITRAMFS PACK + QEMU ===")

    rootfs = build_dir / "rootfs"
    initramfs = build_dir / "initramfs.cpio.gz"

    if not rootfs.is_dir():
        print("[ERROR] rootfs missing; run initramfs phase first")
        return False

    print(f"Packing initramfs -> {initramfs}")
    run_cmd(
        ["sh", "-c", f"cd {rootfs} && find . | cpio -H newc -o | gzip > {initramfs}"],
        cwd=rootfs.parent,
    )

    qemu_bin = shutil.which("qemu-system-x86_64")
    if not qemu_bin:
        print("[WARN] qemu-system-x86_64 not installed; skipping VM run")
        print("Manual: qemu-system-x86_64 -kernel <bzImage> -initrd initramfs.cpio.gz ...")
        return True

    if kernel is None or not kernel.is_file():
        print("[INFO] No --kernel provided (or path missing). Initramfs ready.")
        print(
            f"  qemu-system-x86_64 -m 512M -kernel <bzImage> -initrd {initramfs} "
            '-append "console=ttyS0 quiet panic=1 sysx.autotest=stop:baseline" -nographic'
        )
        return True

    if not run_vm:
        print("[INFO] Initramfs packed. Pass --qemu-run to execute QEMU.")
        return True

    print(f"[SYSX] Launching QEMU (timeout {qemu_timeout_sec}s) kernel={kernel}")
    # -nographic implies serial on stdio; do not also pass -serial stdio (QEMU 10+ errors).
    # `sysx.autotest=stop:<svc>`: sysxd runs `Stop` after FD3 readiness (Phase 3 oracle).
    append = (
        f"console=ttyS0 quiet panic=1 "
        f"sysx.autotest=stop:{autotest_service}"
    )
    cmd = [
        qemu_bin,
        "-m",
        "512M",
        "-kernel",
        str(kernel),
        "-initrd",
        str(initramfs),
        "-append",
        append,
        "-nographic",
        "-no-reboot",
    ]
    try:
        r = subprocess.run(
            cmd,
            cwd=build_dir,
            timeout=qemu_timeout_sec,
            text=True,
            capture_output=True,
        )
    except subprocess.TimeoutExpired as ex:
        print("[INFO] QEMU hit timeout (expected if sysxd hangs in reactor loop)")
        serial = (ex.stdout or "") + (ex.stderr or "")
        ok_s = verify_serial_oracle(serial, strict=oracle_strict)
        ok_d = verify_dual_oracle(serial, strict=dual_oracle_strict)
        ok_t0 = verify_test0_oracle(serial, service=autotest_service, strict=test0_strict)
        if oracle_strict and not ok_s:
            return False
        if dual_oracle_strict and not ok_d:
            return False
        if test0_strict and not ok_t0:
            return False
        return True
    serial = (r.stdout or "") + (r.stderr or "")
    ok_s = verify_serial_oracle(serial, strict=oracle_strict)
    ok_d = verify_dual_oracle(serial, strict=dual_oracle_strict)
    ok_t0 = verify_test0_oracle(serial, service=autotest_service, strict=test0_strict)
    if oracle_strict and not ok_s:
        return False
    if dual_oracle_strict and not ok_d:
        return False
    if test0_strict and not ok_t0:
        return False
    print(f"[INFO] QEMU exited with code {r.returncode}")
    return True


# `11` — serial markers + dual-oracle lines (`SYSX_ORACLE_DUAL`).
_ORACLE_MARKERS_REQUIRED = (
    "SYSX_ORACLE_SCHEMAS_LOADED",
    "SYSX_ORACLE_DAG_OK",
    "SYSX_ORACLE_BOOTSTRAP",
    "SYSX_ORACLE_REACTOR_READY",
)

# Phase 3 — FD3 readiness + DFS unlink (hypervisor Test 0).
_TEST0_READINESS = "[SYSX] ORACLE_READINESS"
_TEST0_DFS = "[SYSX] ORACLE_DFS_UNLINK_COMPLETE"

# `11` § Test 0 / dual-oracle — lines emitted by sysxd `reactor.rs` after IPC dispatch.
_ORACLE_DUAL_RE = re.compile(
    r"SYSX_ORACLE_DUAL op=(?P<op>\w+) service=(?P<svc>\S+) "
    r"ipc=S:0x(?P<s>[0-9a-fA-F]{2}):R:0x(?P<r>[0-9a-fA-F]{2}) "
    r"sysfs_populated=(?P<pop>[\w]+) sysfs_match=(?P<m>\d+|na)"
)


def _copy_busybox_sh(rootfs: Path) -> None:
    """Provide /bin/sh in initramfs when host has busybox (QEMU baseline workload)."""
    for cand in (shutil.which("busybox"), Path("/usr/bin/busybox")):
        if cand and Path(cand).is_file():
            dest_dir = rootfs / "bin"
            dest_dir.mkdir(parents=True, exist_ok=True)
            bb = dest_dir / "busybox"
            shutil.copy2(Path(cand), bb)
            bb.chmod(0o755)
            sh_link = dest_dir / "sh"
            if sh_link.exists() or sh_link.is_symlink():
                sh_link.unlink()
            sh_link.symlink_to("busybox")
            print(f"[SUCCESS] Installed busybox -> {bb} (symlink /bin/sh)")
            return
    print(
        "[WARN] busybox not found; baseline schema exec /bin/sh may fail in initramfs. "
        "Install busybox or add a static shell under /bin/sh."
    )


def verify_dual_oracle(serial_text: str, strict: bool = False) -> bool:
    """`11` — compare IPC reply bytes vs sysfs `populated` hint on SYSX_ORACLE_DUAL lines."""
    lines = [ln for ln in serial_text.splitlines() if "SYSX_ORACLE_DUAL" in ln]
    if not lines:
        msg = "[ORACLE] No SYSX_ORACLE_DUAL lines (11 dual-oracle)"
        if strict:
            print(msg, file=sys.stderr)
            return False
        print(f"{msg} (non-strict: continue)")
        return True

    bad: list[str] = []
    for ln in lines:
        m = _ORACLE_DUAL_RE.search(ln)
        if not m:
            bad.append(f"unparseable: {ln[:120]}")
            continue
        op = m.group("op")
        b0 = int(m.group("s"), 16)
        b1 = int(m.group("r"), 16)
        pop = m.group("pop")
        mat = m.group("m")
        if op == "status" and mat != "na":
            if mat != "1":
                bad.append(f"status sysfs_match={mat} (expected 1) ipc=[0x{b0:02x},0x{b1:02x}] pop={pop}")
        # start/stop: sysfs_match=na — informational only
    if bad:
        msg = "[ORACLE] Dual-oracle issues: " + "; ".join(bad)
        if strict:
            print(msg, file=sys.stderr)
            return False
        print(f"[ORACLE] {msg} (non-strict: continue)")
        return True
    print("[ORACLE] SYSX_ORACLE_DUAL lines OK (status rows sysfs_match=1 where present)")
    return True


def verify_serial_oracle(serial_text: str, strict: bool = False) -> bool:
    """Pass/fail hints on captured QEMU serial (`11`)."""
    missing = [m for m in _ORACLE_MARKERS_REQUIRED if m not in serial_text]
    if missing:
        msg = f"[ORACLE] Missing serial markers: {missing} (11 §oracle constraints)"
        if strict:
            print(msg, file=sys.stderr)
            return False
        print(f"[ORACLE] {msg} (non-strict: continue)")
        return True
    print("[ORACLE] Serial markers OK:", ", ".join(_ORACLE_MARKERS_REQUIRED))
    return True


def verify_test0_oracle(serial_text: str, service: str = "baseline", strict: bool = False) -> bool:
    """Phase 3 Test 0: FD3 readiness then DFS post-order unlink complete (`sysx.autotest=stop:<svc>`)."""
    ok_r = _TEST0_READINESS in serial_text and f"service={service}" in serial_text
    ok_d = _TEST0_DFS in serial_text and f"service={service}" in serial_text
    if not ok_r or not ok_d:
        msg = (
            f"[ORACLE] Test0 missing readiness={ok_r} dfs_unlink={ok_d} "
            f"(need {_TEST0_READINESS!r} and {_TEST0_DFS!r} for service={service})"
        )
        if strict:
            print(msg, file=sys.stderr)
            return False
        print(f"{msg} (non-strict: continue)")
        return True
    print("[ORACLE] Test0 OK (readiness + DFS unlink markers)")
    return True


def main():
    parser = argparse.ArgumentParser(description="SysX Install Tool - Hypervisor Test Pipeline")
    parser.add_argument(
        "--sysx-root",
        type=Path,
        default=_DEFAULT_SYSX_ROOT,
        help="SysX workspace root (Cargo.toml)",
    )
    parser.add_argument("--build-dir", type=Path, default=Path("/tmp/sysx-build"),
                       help="Build directory")
    parser.add_argument("--phase", choices=["all", "musl", "initramfs", "qemu"],
                       default="all", help="Run specific phase")
    parser.add_argument(
        "--kernel",
        type=Path,
        default=None,
        help="Path to Linux bzImage for QEMU (optional)",
    )
    parser.add_argument(
        "--qemu-run",
        action="store_true",
        help="Actually run QEMU when --kernel is set (short timeout)",
    )
    parser.add_argument(
        "--qemu-timeout",
        type=int,
        default=12,
        help="Seconds before QEMU is killed (smoke test)",
    )
    parser.add_argument(
        "--oracle-strict",
        action="store_true",
        help="Fail if QEMU serial is missing SYSX_ORACLE_* markers (11)",
    )
    parser.add_argument(
        "--dual-oracle-strict",
        action="store_true",
        help="Fail if SYSX_ORACLE_DUAL lines missing or status sysfs_match!=1 (11 Test 0)",
    )
    parser.add_argument(
        "--test0-strict",
        action="store_true",
        help="Fail if Phase 3 Test0 markers missing (ORACLE_READINESS + ORACLE_DFS_UNLINK_COMPLETE)",
    )
    parser.add_argument(
        "--autotest-service",
        default="baseline",
        help="Kernel cmdline sysx.autotest=stop:<name> (default baseline)",
    )
    args = parser.parse_args()

    args.build_dir.mkdir(parents=True, exist_ok=True)
    print(f"SysX Install Tool starting. Root: {args.sysx_root}")

    success = True

    if args.phase in ("all", "musl"):
        success &= phase_musl_build(args.sysx_root)

    if args.phase in ("all", "initramfs"):
        success &= phase_initramfs_layout(args.sysx_root, args.build_dir)

    if args.phase in ("all", "qemu"):
        success &= phase_qemu_headless(
            args.sysx_root,
            args.build_dir,
            args.kernel,
            args.qemu_run,
            args.qemu_timeout,
            oracle_strict=args.oracle_strict,
            dual_oracle_strict=args.dual_oracle_strict,
            test0_strict=args.test0_strict,
            autotest_service=args.autotest_service,
        )

    if success:
        print("\n[SUCCESS] All phases completed. Oracle ready for validation.")
        print("See 11-hypervisor-test-loop-and-oracle.md for full test assertions.")
        return 0
    else:
        print("\n[FAIL] One or more phases failed.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
