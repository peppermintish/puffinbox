#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Reproduce the isolated GNU runtime experiment in a disposable Linux builder."""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import struct
import subprocess
import sys


UNICODE_SOURCE_SHA256 = "d3d218b7574f08efe423e9d4b6b539e700046aca4f2a2793d38164ec5a1b00b2"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(arguments: list[str], *, cwd: Path, env: dict[str, str], log: Path) -> None:
    with log.open("x") as output:
        result = subprocess.run(arguments, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(f"Probe phase failed with exit {result.returncode}; inspect {log}.")


def inspect(binary: Path, label: str, output: Path, environment: dict[str, str]) -> None:
    for utility, options, suffix in [("nm", ["-C"], "symbols"), ("readelf", ["-l", "-d", "-V"], "elf"),
                                     ("objdump", ["-T"], "dynamic-symbols")]:
        run([utility, *options, str(binary)], cwd=output, env=environment, log=output / f"{label}-{suffix}.txt")


def capture_source_mapping(output: Path, sysroot: Path, environment: dict[str, str], *, external_openssl: bool) -> dict[str, object]:
    root = Path(__file__).resolve().parents[2]
    sys.path.insert(0, str(root / "scripts"))
    from capture_gnu_sources import capture

    run(["readelf", "-SW", str(output / "server")], cwd=output, env=environment,
        log=output / "server-sections.txt")
    sections = (output / "server-sections.txt").read_text()
    if ".debug_line " not in sections or ".debug_info " not in sections:
        raise RuntimeError("The instrumented executable is missing its source-location sections.")
    library = sysroot / "lib/rustlib/src/rust/library"
    source_files = {str(path): digest(path) for path in sorted(library.rglob("*"))
                    if path.is_file() and (path.suffix in {".rs", ".h", ".c", ".S"}
                                           or "LICENSE" in path.name or "COPYRIGHT" in path.name)}
    notice = sysroot / "share/doc/rust/COPYRIGHT-library.html"
    if not notice.is_file():
        raise RuntimeError("The exact compiler's standard-library notice is missing.")
    shutil.copyfile(notice, output / notice.name)
    compiler_notices = {notice.name: digest(output / notice.name)}
    with (output / "standard-library-source-hashes.json").open("x") as ledger:
        json.dump({"sysroot": str(sysroot), "sourceFiles": source_files, "compilerNotices": compiler_notices,
                   "scope": "Exact source bytes available to this disposable build; no license clearance inferred."},
                  ledger, indent=2)
        ledger.write("\n")
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
         "--filter-platform", "x86_64-unknown-linux-gnu"], cwd=root, env=environment, text=True))
    dependencies = capture(metadata, output / "target/x86_64-unknown-linux-gnu/release/build", root / "Cargo.lock",
                           output / "native-compiler-traces", require_native_compilations=not external_openssl)
    with (output / "dependency-source-hashes.json").open("x") as ledger:
        json.dump(dependencies, ledger, indent=2)
        ledger.write("\n")
    return {"sourceMappingPresent": True, "debugLevel": 2, "sourceFileCount": len(source_files),
            "compilerNotices": compiler_notices,
            "dependencyPackages": len(dependencies["packages"]),
            "dependencySourceFiles": len(dependencies["sourceFiles"]),
            "generatedSourceFiles": len(dependencies["generatedSourceFiles"]),
            "dependencySourceHashesSha256": digest(output / "dependency-source-hashes.json"),
            "sourceHashesSha256": digest(output / "standard-library-source-hashes.json"), "licenseClearance": False}


def startup_check(output: Path, environment: dict[str, str]) -> dict[str, object]:
    fixture_environment = {name: value for name, value in environment.items()
                           if name not in {"DATABASE_URL", "PUFFINBOX_DATABASE_URL"}}
    fixture_environment["LD_LIBRARY_PATH"] = str(output / "external-runtime")
    result = subprocess.run([str(output / "server")], cwd=output, env=fixture_environment,
                            capture_output=True, text=True, timeout=30)
    with (output / "startup-check.log").open("x") as log:
        log.write(result.stdout + result.stderr)
    if result.returncode != 1 or "DATABASE_URL is required" not in result.stdout + result.stderr:
        raise RuntimeError("The executable did not reach and reject the missing-database configuration.")
    return {"executableMode": oct((output / "server").stat().st_mode & 0o777),
            "missingDatabaseExitCode": result.returncode, "configurationErrorReached": True}


def c_header_check(output: Path, root: Path, environment: dict[str, str]) -> dict[str, object]:
    header = root / "experiments/linux-gnu-runtime/c-headers.h"
    fixture = root / "experiments/linux-gnu-runtime/c-headers.c"
    for label in ["baseline", "adapter"]:
        command = ["cc", "-O2", "-g", "-Wall", "-Wextra", "-Werror"]
        if label == "adapter":
            command += ["-D_GNU_SOURCE", "-D__NO_INLINE__", "-include", str(header)]
        binary = output / f"c-headers-{label}"
        run([*command, str(fixture), "-o", str(binary)], cwd=output, env=environment,
            log=output / f"c-headers-{label}-build.log")
        run([str(binary), " -12345tail"], cwd=output, env=environment,
            log=output / f"c-headers-{label}-output.txt")
    before = (output / "c-headers-baseline-output.txt").read_bytes()
    after = (output / "c-headers-adapter-output.txt").read_bytes()
    if before != after or len(after.splitlines()) != 4096:
        raise RuntimeError("The endian adapter differed from the C baseline or independent byte oracle.")
    assembly = output / "c-headers-assembly.S"
    assembly.write_text(".text\n.p2align 4\n.globl puffinbox_c_header_assembly_probe\npuffinbox_c_header_assembly_probe:\n    ret\n.section .note.GNU-stack,\"\",@progbits\n")
    run(["cc", "-D_GNU_SOURCE", "-D__NO_INLINE__", "-include", str(header), "-c", str(assembly),
         "-o", str(output / "c-headers-assembly.o")], cwd=output, env=environment,
        log=output / "c-headers-assembly-build.log")
    return {"identicalOutputs": True, "inputRows": 4096,
            "assemblyPreprocessingPassed": True,
            "outputSha256": hashlib.sha256(after).hexdigest(),
            "scope": "Native x86-64 endian conversions, single evaluation, and an external atoi call."}


def numeric_check(output: Path, root: Path, environment: dict[str, str]) -> dict[str, object]:
    captured = json.loads((output / "puffinbox_server-rustc-argv.json").read_text())
    # Use the same explicitly rebuilt std dependency and GNU target. The
    # baseline retains compiler-builtins; the alternative excludes its archive.
    shared = next((output / "external-runtime").glob("libstd-*.so"))
    dependencies = output / "target/x86_64-unknown-linux-gnu/release/deps"
    base = [captured["compiler"], "--edition=2024", "--crate-name", "puffinbox_numeric_probe",
            "--target", "x86_64-unknown-linux-gnu", "-C", "opt-level=3", "-C", "prefer-dynamic",
            "--extern", "std=" + str(shared), "-L", "dependency=" + str(dependencies),
            "-C", "link-arg=-nostartfiles", "-C", "link-arg=-Wl,--wrap=atexit,--wrap=pthread_atfork",
            "-C", "link-arg=" + str(output / "entry.o"), "-C", "link-arg=" + str(output / "compat.o"),
            str(root / "experiments/linux-gnu-runtime/numeric.rs")]
    # A direct rustc invocation otherwise imports its stock core crate as well
    # as the rebuilt core used by this std, producing duplicate language items.
    base += ["-Z", "unstable-options"]
    for index, argument in enumerate(captured["original"][:-1]):
        dependency = captured["original"][index + 1]
        if argument == "--extern" and dependency.startswith("noprelude,nounused:") and not dependency.startswith("noprelude,nounused:std="):
            base += ["--extern", dependency]
    for label in ["baseline", "external"]:
        arguments = [*base, "-o", str(output / f"numeric-{label}")]
        phase_environment = {**environment, "PUFFINBOX_RUNTIME_LINK_LABEL": label,
                             "LD_LIBRARY_PATH": str(output / "external-runtime")}
        if label == "external":
            arguments += ["-C", "linker=" + str(root / "experiments/linux-gnu-runtime/linker-wrapper.py")]
        run(arguments, cwd=root, env=phase_environment, log=output / f"numeric-{label}-build.log")
        inspect(output / f"numeric-{label}", "numeric-" + label, output, phase_environment)
        run([str(output / f"numeric-{label}"), "18446744073709551629"], cwd=output,
            env=phase_environment, log=output / f"numeric-{label}-output.txt")
    helpers = {"round", "floor", "ceil", "trunc", "rint", "floorf", "ceilf", "truncf", "rintf", "__floattidf", "__umodti3"}
    baseline_symbols = (output / "numeric-baseline-symbols.txt").read_text().splitlines()
    external_symbols = (output / "numeric-external-symbols.txt").read_text().splitlines()
    baseline_definitions = {line.split()[-1] for line in baseline_symbols if re.match(r"^[0-9a-f]+ [tTwW] ", line)}
    external_imports = {line.split()[-1] for line in external_symbols if re.match(r"^\s+U ", line)}
    if not helpers.issubset(baseline_definitions) or not helpers.issubset(external_imports):
        raise RuntimeError("The numeric baseline and alternative did not exercise all 11 distinct helper providers.")
    before = (output / "numeric-baseline-output.txt").read_bytes()
    after = (output / "numeric-external-output.txt").read_bytes()
    rows = [line.split() for line in after.decode().splitlines() if line.startswith("i ")]
    conversion_passed = True
    for row in rows:
        value = int(row[1])
        signed = value - (1 << 128) if value >= 1 << 127 else value
        conversion_passed &= row[4] == struct.pack(">d", float(signed)).hex()
    record = {"identicalOutputs": before == after, "helperProvidersVerified": True, "integerRows": len(rows),
              "moduloOraclePassed": all(int(row[3]) == int(row[1]) % int(row[2]) for row in rows),
              "signedFloatOraclePassed": conversion_passed, "outputSha256": hashlib.sha256(after).hexdigest()}
    if len(rows) != 4096 or not all(record[name] for name in ["identicalOutputs", "moduloOraclePassed", "signedFloatOraclePassed"]):
        raise RuntimeError("Numeric differential or independent integer oracle failed.")
    return record


def tls_check(output: Path, root: Path, environment: dict[str, str]) -> dict[str, object]:
    captured = json.loads((output / "puffinbox_server-rustc-argv.json").read_text())
    source = root / "examples/tls_probe.rs"
    checker = root / "scripts/check_tls.py"
    binary = output / "tls-client"
    arguments = [captured["compiler"], "--edition=2024", "--crate-name", "puffinbox_tls_probe",
                 "--target", "x86_64-unknown-linux-gnu", "-C", "opt-level=3", "-C", "prefer-dynamic",
                 "-Z", "unstable-options", "-C", "linker=" + str(root / "experiments/linux-gnu-runtime/linker-wrapper.py"),
                 "-C", "link-arg=-nostartfiles", "-C", "link-arg=-Wl,--wrap=atexit,--wrap=pthread_atfork",
                 "-C", "link-arg=" + str(output / "entry.o"), "-C", "link-arg=" + str(output / "compat.o"),
                 str(source), "-o", str(binary)]
    # Host dependency paths are needed by Tokio's procedural macro; target
    # paths and explicit rebuilt std dependencies must match the server.
    for index, argument in enumerate(captured["modified"][:-1]):
        value = captured["modified"][index + 1]
        if argument == "-L" and value.startswith("dependency="):
            arguments += ["-L", value]
        if argument == "--extern" and value.startswith(("noprelude,nounused:", "std=", "reqwest=", "tokio=")):
            arguments += ["--extern", value]
    phase_environment = {**environment, "PUFFINBOX_RUNTIME_LINK_LABEL": "tls-client",
                         "LD_LIBRARY_PATH": str(output / "external-runtime")}
    run(arguments, cwd=root, env=phase_environment, log=output / "tls-client-build.log")
    binary.chmod(0o555)
    run([sys.executable, str(checker), "--probe", str(binary)], cwd=root, env=phase_environment,
        log=output / "tls-client-check.log")
    record = {"passed": True, "certificateCases": 4, "binarySha256": digest(binary),
              "sourceSha256": digest(source), "checkerSha256": digest(checker),
              "serverSha256": digest(output / "server"), "licenseClearance": False,
              "scope": "Rust TLS helper reuses the server dependency artifacts and rebuilt shared std; this is not a server HTTP test."}
    with (output / "tls-client-check.json").open("x") as ledger:
        json.dump(record, ledger, indent=2)
        ledger.write("\n")
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="new output directory outside the mounted source and sysroot")
    parser.add_argument("--source-map", action="store_true", help="retain debug locations and source/package/notice hashes")
    parser.add_argument("--external-openssl", action="store_true", help="use operator-supplied OpenSSL 3 shared libraries")
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() != "x86_64" or not Path("/.dockerenv").is_file():
        parser.error("Run inside a disposable Linux x86-64 Docker builder; this experiment changes its rust-src copy.")
    root = Path(__file__).resolve().parents[2]
    output = args.output.resolve()
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    if output.exists() or output.is_relative_to(root) or output.is_relative_to(sysroot) or root.is_relative_to(output) or sysroot.is_relative_to(output):
        parser.error("Output must be new and outside the source mount and compiler sysroot.")
    version = subprocess.check_output(["rustc", "--version"], text=True).strip()
    if not version.startswith("rustc 1.98.1 "):
        parser.error("This probe is pinned to Rust 1.98.1.")
    if any(os.environ.get(name) for name in ["RUSTC_WRAPPER", "RUSTFLAGS", "CFLAGS", "CC_SHELL_ESCAPED_FLAGS", "CC",
                                            "OPENSSL_NO_VENDOR", "OPENSSL_STATIC", "OPENSSL_DIR", "OPENSSL_LIB_DIR", "OPENSSL_INCLUDE_DIR"]):
        parser.error("Use a builder without preexisting compiler wrappers, Rust flags, or C flags.")
    source = sysroot / "lib/rustlib/src/rust/library/core/src/unicode/unicode_data.rs"
    if not source.is_file() or digest(source) != UNICODE_SOURCE_SHA256:
        parser.error("The rust-src Unicode file does not match the recorded original Rust 1.98.1 input.")
    output.mkdir(parents=True, exist_ok=False)
    record = {"startedAtUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "compiler": version,
              "externalOpenSslRequested": args.external_openssl,
              "productionChanged": False, "licenseClearance": False, "sourceMountedAt": str(root),
              "cargoLockSha256": digest(root / "Cargo.lock"),
              "auditScriptsSha256": {name: digest(root / "scripts" / name)
                                      for name in ["capture_gnu_sources.py", "check_dependency_replacements.py", "check_gnu_link_map.py", "check_gnu_shared_tls.py", "check_gnu_source_map.py", "check_gnu_notices.py", "check_tls.py", "openssl-configure.py", "check_openssl_exclusions.py"]},
              "cargoConfigSha256": digest(root / ".cargo/config.toml"),
              "dependencyReplacementReviewSha256": digest(root / "vendor/dependency-replacements.json"),
              "tlsProbeSourceSha256": digest(root / "examples/tls_probe.rs"),
              "experimentSourceSha256": {name: digest(Path(__file__).with_name(name))
                                         for name in ["build.py", "compiler-wrapper.py", "native-source-wrapper.py", "linker-wrapper.py", "numeric.rs", "c-headers.h", "c-headers.c"]}}
    result = 1
    try:
        original = source.read_text()
        changed = original.replace("#[inline(always)]", "#[inline(never)]").replace("#[inline]", "#[inline(never)]")
        if original.count("#[inline(always)]") + original.count("#[inline]") != 13:
            raise RuntimeError("Unexpected generated Unicode function attributes.")
        source.write_text(changed)
        record["stdlibSourceChange"] = {"originalSha256": UNICODE_SOURCE_SHA256, "modifiedSha256": digest(source),
                                       "attributesChanged": 13, "tablesChanged": False}
        environment = {**os.environ, "CARGO_BUILD_JOBS": "2", "RUSTC_BOOTSTRAP": "1", "RUSTFLAGS": "-C prefer-dynamic",
                       "RUSTC_WRAPPER": str(root / "experiments/linux-gnu-runtime/compiler-wrapper.py"),
                       "PUFFINBOX_RUNTIME_PROBE_OUTPUT": str(output),
                       "CC": str(root / "experiments/linux-gnu-runtime/native-source-wrapper.py"),
                       "CFLAGS": shlex.join(["-D_GNU_SOURCE", "-D__NO_INLINE__", "-include", str(root / "experiments/linux-gnu-runtime/c-headers.h")]),
                       "CC_SHELL_ESCAPED_FLAGS": "1"}
        record["cHeaderFlags"] = environment["CFLAGS"]
        if args.external_openssl:
            environment["OPENSSL_NO_VENDOR"] = "1"
            tls_version = subprocess.check_output(["pkg-config", "--modversion", "openssl"], env=environment, text=True).strip()
            if not re.fullmatch(r"3[.][0-9]+[.][0-9]+", tls_version):
                raise RuntimeError("The external TLS experiment requires an OpenSSL 3 development installation.")
            record["externalOpenSslBuildVersion"] = tls_version
        run([sys.executable, str(root / "scripts/check_dependency_replacements.py"),
             "--target", "x86_64-unknown-linux-gnu"], cwd=root, env=environment,
            log=output / "dependency-replacements-check.log")
        record["dependencyReplacementCheckSha256"] = digest(output / "dependency-replacements-check.log")
        record["cHeaderCheck"] = c_header_check(output, root, environment)
        if args.source_map:
            environment["CARGO_PROFILE_RELEASE_DEBUG"] = "2"
        for name in ["entry", "compat"]:
            run(["cc", "-c", str(root / f"experiments/linux-gnu-entry/{name}.S"), "-o", str(output / f"{name}.o")],
                cwd=root, env=environment, log=output / f"{name}-build.log")
        run(["cargo", "-Zbuild-std=std,panic_unwind", "rustc", "--locked", "--release", "--target", "x86_64-unknown-linux-gnu",
             "--target-dir", str(output / "target"), "--bin", "puffinbox-server", "--", "-C", "link-arg=-nostartfiles",
             "-C", "link-arg=-Wl,--wrap=atexit,--wrap=pthread_atfork", "-C", "link-arg=" + str(output / "entry.o"),
             "-C", "link-arg=" + str(output / "compat.o"), "-C", "link-arg=-Wl,-Map," + str(output / "server-link.map") + ",--cref"],
            cwd=root, env=environment, log=output / "cargo-build.log")
        if digest(root / "Cargo.lock") != record["cargoLockSha256"]:
            raise RuntimeError("The project lockfile changed during the experiment.")
        shutil.copyfile(output / "target/x86_64-unknown-linux-gnu/release/puffinbox-server", output / "server")
        (output / "server").chmod(0o555)
        runtime = output / "external-runtime"
        runtime.mkdir()
        libraries = list((output / "target/x86_64-unknown-linux-gnu/release/deps").glob("libstd-*.so"))
        if len(libraries) != 1:
            raise RuntimeError("Expected one rebuilt shared standard library.")
        shutil.copyfile(libraries[0], runtime / libraries[0].name)
        record["binarySha256"] = digest(output / "server")
        record["externalStdlibSha256"] = digest(runtime / libraries[0].name)
        record["startupCheck"] = startup_check(output, environment)
        shutil.copyfile(sysroot / "lib/rustlib/src/rust/library/Cargo.lock", output / "standard-library-Cargo.lock")
        inspect(output / "server", "server", output, environment)
        run([sys.executable, str(root / "scripts/check_gnu_link_map.py"), "--map", str(output / "server-link.map"),
             "--symbols", str(output / "server-symbols.txt"), "--output", str(output / "runtime-input-inventory.json")],
            cwd=root, env=environment, log=output / "inventory-check.log")
        if args.external_openssl:
            sys.path.insert(0, str(root / "scripts"))
            from check_gnu_shared_tls import inventory as tls_inventory

            tls = tls_inventory((output / "server-elf.txt").read_text(), (output / "server-symbols.txt").read_text(),
                                json.loads((output / "runtime-input-inventory.json").read_text()))
            with (output / "shared-tls-inventory.json").open("x") as ledger:
                json.dump(tls, ledger, indent=2)
                ledger.write("\n")
            record["sharedTlsInventorySha256"] = digest(output / "shared-tls-inventory.json")
        record["numericCheck"] = numeric_check(output, root, environment)
        record["tlsCheck"] = tls_check(output, root, environment)
        if args.source_map:
            record["sourceMapping"] = capture_source_mapping(output, sysroot, environment, external_openssl=args.external_openssl)
        if not args.external_openssl:
            exclusion = [sys.executable, str(root / "scripts/check_openssl_exclusions.py"),
                         "--binary", str(output / "server"), "--map", str(output / "server-link.map"),
                         "--output", str(output / "openssl-exclusions.json")]
            if args.source_map:
                exclusion += ["--native-sources", str(output / "dependency-source-hashes.json")]
            run(exclusion, cwd=root, env=environment, log=output / "openssl-exclusions.log")
            record["openSslExclusionsSha256"] = digest(output / "openssl-exclusions.json")
        result = 0
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        record["failure"] = str(error)
    finally:
        record.update({"finishedAtUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "exitCode": result})
        with (output / "probe.json").open("x") as ledger:
            json.dump(record, ledger, indent=2)
            ledger.write("\n")
    print(f"Experimental GNU runtime: {'passed' if result == 0 else 'failed'}; evidence in {output}.", flush=True)
    return result


if __name__ == "__main__":
    raise SystemExit(main())
