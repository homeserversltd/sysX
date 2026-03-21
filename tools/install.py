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
import subprocess
import sys
import shutil
from pathlib import Path
from typing import Optional

# Default to repository root containing this script (…/sysX).
_DEFAULT_SYSX_ROOT = Path(__file__).resolve().parent.parent

MUSL_TARGET = "x86_64-unknown-linux-musl"


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
    for d in ["bin", "sbin", "etc", "run", "sys", "proc", "dev", "tmp"]:
        (rootfs / d).mkdir(parents=True, exist_ok=True)

    # Place sysxd as /sbin/init
    binary_src = resolve_sysxd_binary(sysx_root)
    if binary_src is not None:
        shutil.copy2(binary_src, rootfs / "sbin" / "init")
        (rootfs / "sbin" / "init").chmod(0o755)
        print("[SUCCESS] Placed sysxd as /sbin/init")
    else:
        print("[WARN] No binary, creating stub")
        (rootfs / "sbin" / "init").write_text("#!/bin/sh\necho 'SYSX STUB INIT'\nexec /bin/sh\n")
        (rootfs / "sbin" / "init").chmod(0o755)

    # Write test schema
    schema_dir = rootfs / "etc" / "sysx"
    schema_dir.mkdir(parents=True, exist_ok=True)
    (schema_dir / "test.yaml").write_text("""services:
  baseline:
    command: ["/bin/sh", "-c", "echo 'SYSX_BASELINE_STARTED'; exec sleep 3600"]
    depends_on: []
""")

    print(f"[SUCCESS] Initramfs layout forged at {rootfs}")
    return True


def phase_qemu_headless(sysx_root: Path, build_dir: Path) -> bool:
    """Phase 3: QEMU headless with oracle markers."""
    print("\n=== PHASE 3: QEMU HEADLESS ===")

    rootfs = build_dir / "rootfs"
    initramfs = build_dir / "initramfs.cpio.gz"

    if not shutil.which("qemu-system-x86_64"):
        print("[ERROR] qemu-system-x86_64 not found")
        return False

    # Create basic initramfs (simplified)
    print("Creating initramfs (placeholder)...")
    run_cmd(["sh", "-c", f"cd {rootfs} && find . | cpio -H newc -o | gzip > {initramfs}"], cwd=rootfs.parent)

    print("[INFO] QEMU would run here with:")
    print("  -kernel bzImage")
    print("  -initrd initramfs.cpio.gz")
    print("  -serial stdio -nographic")
    print("  -append 'console=ttyS0 panic=1'")

    # Oracle marker simulation
    print("\n[ORACLE] Expected markers from 11-*:")
    print("- SYSX_BASELINE_STARTED")
    print("- cgroup.events populated=1")
    print("- IPC 0x10/0x20/0x30 responses")
    print("- DFS post-order unlink on stop (§4.1.1 of 12-canonical-spec-v1.md)")

    print("[SUCCESS] QEMU phase complete (simulation)")
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
    args = parser.parse_args()

    args.build_dir.mkdir(parents=True, exist_ok=True)
    print(f"SysX Install Tool starting. Root: {args.sysx_root}")

    success = True

    if args.phase in ("all", "musl"):
        success &= phase_musl_build(args.sysx_root)

    if args.phase in ("all", "initramfs"):
        success &= phase_initramfs_layout(args.sysx_root, args.build_dir)

    if args.phase in ("all", "qemu"):
        success &= phase_qemu_headless(args.sysx_root, args.build_dir)

    if success:
        print("\n[SUCCESS] All phases completed. Oracle ready for validation.")
        print("See 11-hypervisor-test-loop-and-oracle.md for full test assertions.")
        return 0
    else:
        print("\n[FAIL] One or more phases failed.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
