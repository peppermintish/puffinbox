# Shared GNU runtime experiment

This experiment tests a server executable with its Rust and GNU runtime supplied externally. It does not change the production Dockerfile or clear the release license gate. The generated shared standard library contains Unicode data and compiler-builtins under licenses outside the project's allowlist. It must not be included in a permissive-only release bundle.

The build uses the [original process entry and registration bridges](../linux-gnu-entry/README.md). In a disposable builder's Rust 1.98.1 source, it changes 13 generated Unicode function attributes to prevent their inlining into the executable; the Unicode tables and behavior are unchanged. Cargo's experimental `build-std` producer is wrapped to emit a shared standard library. The final executable link excludes the compiler-builtins archive and resolves its helper calls through external `libm.so.6` and `libgcc_s.so.1` instead. These are build experiments using `RUSTC_BOOTSTRAP=1`, not a supported stable build configuration.

The public [GCC floating conversion](https://gcc.gnu.org/onlinedocs/gccint/Soft-float-library-routines.html) and [integer arithmetic](https://gcc.gnu.org/onlinedocs/gccint/Integer-library-routines.html) interfaces describe the helper boundary. The wrapper preserves Cargo's jobserver descriptors and fails if the compiler changes to an uninspected response-file format or selects an unexpected archive.

Run only in a disposable Linux x86-64 Docker builder with Rust 1.98.1, `rust-src`, Python 3, a C compiler, binutils, Perl, and Make. Mount the repository read-only at `/work` and a fresh output parent at `/probe`. From `/work`:

```sh
python3 experiments/linux-gnu-runtime/build.py --output /probe/output
```

The [probe Dockerfile](Dockerfile) installs those build prerequisites. Build it with `docker build --tag puffinbox:gnu-probe-builder --file experiments/linux-gnu-runtime/Dockerfile .`, then run that image with the two mounts and `--output /probe/output`. The separate experimental CI workflow repeats this recipe and saves only inventory records, excluding executable and runtime binaries.

The script rejects a host invocation, an existing output directory, a changed Unicode source input, or a different compiler version. Cargo retains the project lockfile. Dependency downloads and compiler-source changes stay in the disposable builder. The copied server has mode 0555. A native Linux startup check must reach and reject the missing database configuration, catching lost executable permissions and loader failures before numerical tests. The output records compiler arguments, actual linker inputs, maps, symbols, dynamic dependencies, hashes, and a separate standard-library lockfile. Do not treat the output directory or builder image as a release artifact.

The numeric fixture compares all 11 previously retained helper functions against a baseline that includes compiler-builtins. It covers signed zero, subnormals, infinities, NaNs, halfway rounding values, and 4,096 deterministic integer and floating inputs. Python integer arithmetic and conversion independently check the 128-bit modulo and signed-to-double results. Unicode trimming, alphabetic and numeric classification, and case conversion are also exercised.

The C build disables optional GNU header inlining and uses an [original endian adapter](c-headers.h) based on GCC's documented [byte-swap builtins](https://gcc.gnu.org/onlinedocs/gcc/Byte-Swapping-Builtins.html). Public system headers supply types and declarations. The adapter supplies byte-order conversions with fixed-width casts and single evaluation; `atoi` remains an external call. The [C fixture](c-headers.c) compares 4,096 deterministic inputs with both the ordinary C baseline and an independent byte-loop oracle. It also checks the GNU datagram types used by OpenSSL. `_GNU_SOURCE` is set before the forced include so those declarations remain available, and the adapter skips preprocessed assembly. A separate assembly fixture catches accidental C declarations in assembler input.

The link-map checker reports known retained runtime sections and generated Unicode definitions. It rejects an empty or unsupported map instead of inferring absence from it. A clean inventory is evidence for this particular executable, not complete license clearance: inlined source provenance, all bundled inputs, exact external-runtime requirements, packaging, and broader behavior remain separate release checks. See [the runtime audit](../../docs/runtime-link-audit.md).

## Source-location inventory

Pass `--source-map` to retain release debug data and record exact standard-library source hashes. This produces a larger test executable and does not change production packaging:

```sh
python3 experiments/linux-gnu-runtime/build.py --output /probe/output --source-map
```

The external [pyelftools inspector](https://github.com/eliben/pyelftools) reads DWARF source locations. CI installs version 0.33 with its wheel hash pinned; it is build infrastructure and is excluded from project runtime dependencies and release bundles. With that inspector available in a separate audit environment:

```sh
python3 scripts/check_gnu_source_map.py \
  --binary /probe/output/server \
  --source-hashes /probe/output/standard-library-source-hashes.json \
  --output /probe/output/source-line-inventory.json
```

The checker resolves DWARF 4 and 5 file indexes, attributes only nonempty intervals inside executable load ranges, and requires hashes for every mapped standard-library source. Missing debug information, unresolved paths, empty mappings, and missing hashes fail. Known non-allowlisted mapped source paths also fail the check, including generated Unicode even if the link map names no Unicode archive. Mapped instructions from `/usr/include/` fail as unreviewed system-header inputs. This catches header code that an archive-only inventory misses; it does not assign one license to all system headers. The inventory records line-zero and discarded-address intervals separately. Its byte counts can overlap; they are not a coverage percentage. Anonymous constants, unmapped instructions, assembler, generated code, headers without mapped instructions, and complete source-license classification remain outside this check. Every record keeps `licenseClearance: false`.
