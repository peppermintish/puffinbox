#!/usr/bin/env python3
"""Exercise the experimental original Linux x86-64 process entry and bridges."""

from __future__ import annotations

import os
import platform
import re
import subprocess
import tempfile
from pathlib import Path


def run(args: list[str], *, env: dict[str, str] | None = None, expected: int = 0) -> str:
    completed = subprocess.run(args, env=env, capture_output=True, text=True, timeout=60)
    if completed.returncode != expected:
        raise RuntimeError(f"Runtime entry check failed ({completed.returncode}, expected {expected}):\n{completed.stdout}\n{completed.stderr}")
    return completed.stdout


def main() -> int:
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("The experimental entry check requires Linux x86-64 with glibc 2.34 or newer.")
    libc, version = platform.libc_ver()
    if libc != "glibc" or tuple(int(part) for part in version.split(".")[:2]) < (2, 34):
        raise SystemExit("The NULL-init entry contract requires glibc 2.34 or newer.")
    root = Path(__file__).resolve().parents[1]
    sources = root / "experiments/linux-gnu-entry"
    sysroot = Path(run(["rustc", "--print", "sysroot"]).strip())
    stdlib = sysroot / "lib/rustlib/x86_64-unknown-linux-gnu/lib"
    if len(list(stdlib.glob("libstd-*.so"))) != 1:
        raise SystemExit("The selected Rust toolchain must provide one shared GNU standard library.")

    with tempfile.TemporaryDirectory(prefix="puffinbox-entry-check-") as temporary:
        output = Path(temporary)
        entry = output / "entry.o"
        compat = output / "compat.o"
        run(["cc", "-c", str(sources / "entry.S"), "-o", str(entry)])
        run(["cc", "-c", str(sources / "compat.S"), "-o", str(compat)])
        run(["cc", "-shared", "-fPIC", "-nostartfiles", "-Wl,-z,relro,-z,now",
             str(sources / "shared.c"), "-o", str(output / "libstartup-test.so")])
        link = ["-fPIE", "-pie", "-nostartfiles", "-Wl,--wrap=atexit,--wrap=pthread_atfork,-z,relro,-z,now",
                str(entry), str(compat), "-pthread"]
        lifecycle = output / "lifecycle"
        lifecycle_map = output / "lifecycle.map"
        run(["cc", *link, str(sources / "lifecycle.c"), "-L"+str(output), "-lstartup-test",
             "-Wl,-rpath,"+str(output), "-Wl,-Map,"+str(lifecycle_map), "-o", str(lifecycle)])
        environment = {**os.environ, "PUFFINBOX_STARTUP_PROBE": "original-entry", "LD_LIBRARY_PATH": str(stdlib)}
        lifecycle_output = run([str(lifecycle), "one", "two"], env=environment, expected=37).splitlines()
        expected = ["shared-init", "exe-init", "main-args-env", "worker-tls", "atexit", "cxa-exit", "exe-fini", "shared-fini"]
        if lifecycle_output != expected:
            raise RuntimeError(f"Unexpected initialization or cleanup order: {lifecycle_output!r}")
        # An archive member in a GNU map denotes extraction. Merely listing
        # the libc linker script's LOAD command does not establish extraction.
        if re.search(r"libc_nonshared\.a\([^)]*\)", lifecycle_map.read_text()):
            raise RuntimeError("The lifecycle fixture still selects a libc nonshared wrapper.")

        fork = output / "fork"
        run(["cc", *link, str(sources / "fork.c"), "-o", str(fork)])
        if run([str(fork)], env=environment).strip() != "prepare-LIFO-parent-child-FIFO":
            raise RuntimeError("Fork callbacks did not run in their required order.")

        rust = output / "rust-lifecycle"
        run(["rustc", "--edition=2024", "-C", "prefer-dynamic", "-C", "link-arg=-nostartfiles",
             "-C", "link-arg=-Wl,--wrap=atexit,--wrap=pthread_atfork",
             "-C", "link-arg="+str(entry), "-C", "link-arg="+str(compat),
             str(sources / "lifecycle.rs"), "-o", str(rust)])
        if run([str(rust), "one", "two"], env=environment).splitlines() != [
            "rust-init-args-env-thread-tls-unwind", "rust-fini",
        ]:
            raise RuntimeError("The Rust initialization, threading, panic or cleanup fixture failed.")

    print("Experimental GNU entry: C lifecycle, fork order, and Rust lifecycle checks passed.")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        raise SystemExit(str(error)) from error
