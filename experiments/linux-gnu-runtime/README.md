# Shared GNU runtime experiment

This experiment tests a server executable with its Rust and GNU runtime supplied externally. It does not change the production Dockerfile or clear the release license gate. The generated shared standard library contains Unicode data and compiler-builtins under licenses outside the project's allowlist. It must not be included in a permissive-only release bundle.

The build uses the [original process entry and registration bridges](../linux-gnu-entry/README.md). In a disposable builder's Rust 1.98.1 source, it changes 13 generated Unicode function attributes to prevent their inlining into the executable; the Unicode tables and behavior are unchanged. Cargo's experimental `build-std` producer is wrapped to emit a shared standard library. The final executable link excludes the compiler-builtins archive and resolves its helper calls through external `libm.so.6` and `libgcc_s.so.1` instead. These are build experiments using `RUSTC_BOOTSTRAP=1`, not a supported stable build configuration.

The public [GCC floating conversion](https://gcc.gnu.org/onlinedocs/gccint/Soft-float-library-routines.html) and [integer arithmetic](https://gcc.gnu.org/onlinedocs/gccint/Integer-library-routines.html) interfaces describe the helper boundary. The wrapper preserves Cargo's jobserver descriptors and fails if the compiler changes to an uninspected response-file format or selects an unexpected archive.

Run only in a disposable Linux x86-64 Docker builder with Rust 1.98.1, `rust-src`, Python 3, a C compiler, binutils, Perl, and Make. Mount the repository read-only at `/work` and a fresh output parent at `/probe`. From `/work`:

```sh
python3 experiments/linux-gnu-runtime/build.py --output /probe/output
```

The [probe Dockerfile](Dockerfile) installs those build prerequisites. Build it with `docker build --tag puffinbox:gnu-probe-builder --file experiments/linux-gnu-runtime/Dockerfile .`, then run that image with the two mounts and `--output /probe/output`. The separate experimental CI workflow repeats this recipe and saves only inventory records, excluding executable and runtime binaries.

Pass `--external-openssl` to use the builder's OpenSSL 3 development installation through `pkg-config`, with `OPENSSL_NO_VENDOR=1`. This follows the [Rust OpenSSL build interface](https://docs.rs/openssl/0.10.81/openssl/). The server must declare `libssl.so.3` and `libcrypto.so.3`, import the required TLS entry points, and retain no named static TLS archives or defined native TLS symbols. The output does not copy these shared libraries. An operator must supply the matching runtime. CI builds both this mode and the default vendored mode:

```sh
python3 experiments/linux-gnu-runtime/build.py --output /probe/output --source-map --external-openssl
```

Both modes build a Rust TLS helper with the server's exact dependency artifacts and rebuilt standard library. A local certificate fixture requires a trusted certificate with the right host to succeed and rejects a wrong CA, wrong host, and both together. The fixture removes its temporary certificates and keys. CI retains the helper's hashes and sanitized check results, excluding the helper binary. This checks outbound TLS verification separately from server HTTPS acceptance.

Vendored OpenSSL uses the repository Configure wrapper with `no-siphash` and
`no-quic`. The selected SipHash source has a separate CC0 reference notice;
disabling only its public provider still retains it through internal QUIC. The
recipe checks the final unstripped server, link map and successful native source
capture and records `openssl-exclusions.json`. External OpenSSL mode uses the
operator's shared libraries and retains its separate shared-TLS inventory.
Neither result clears whole-runtime licensing.

The script rejects a host invocation, an existing output directory, a changed Unicode source input, or a different compiler version. Cargo retains the project lockfile. Dependency downloads and compiler-source changes stay in the disposable builder. The copied server has mode 0555. A native Linux startup check must reach and reject the missing database configuration, catching lost executable permissions and loader failures before numerical tests. The output records compiler arguments, actual linker inputs, maps, symbols, dynamic dependencies, hashes, and a separate standard-library lockfile. Do not treat the output directory or builder image as a release artifact.

The numeric fixture compares all 11 previously retained helper functions against a baseline that includes compiler-builtins. It covers signed zero, subnormals, infinities, NaNs, halfway rounding values, and 4,096 deterministic integer and floating inputs. Python integer arithmetic and conversion independently check the 128-bit modulo and signed-to-double results. Unicode trimming, alphabetic and numeric classification, and case conversion are also exercised.

The C build disables optional GNU header inlining and uses an [original endian adapter](c-headers.h) based on GCC's documented [byte-swap builtins](https://gcc.gnu.org/onlinedocs/gcc/Byte-Swapping-Builtins.html). Public system headers supply types and declarations. The adapter supplies byte-order conversions with fixed-width casts and single evaluation; `atoi` remains an external call. The [C fixture](c-headers.c) compares 4,096 deterministic inputs with both the ordinary C baseline and an independent byte-loop oracle. It also checks the GNU datagram types used by OpenSSL. `_GNU_SOURCE` is set before the forced include so those declarations remain available, and the adapter skips preprocessed assembly. A separate assembly fixture catches accidental C declarations in assembler input.

The link-map checker reports known retained runtime sections and generated Unicode definitions. It rejects an empty or unsupported map instead of inferring absence from it. A clean inventory is evidence for this particular executable, not complete license clearance: inlined source provenance, all bundled inputs, exact external-runtime requirements, packaging, and broader behavior remain separate release checks. See [the runtime audit](../../docs/runtime-link-audit.md).

## Source-location inventory

Pass `--source-map` to retain release debug data, record standard-library and dependency source hashes, and capture the same compiler's `COPYRIGHT-library.html` notice. The hash record binds that notice to this build. This produces a larger test executable and does not change production packaging:

```sh
python3 experiments/linux-gnu-runtime/build.py --output /probe/output --source-map
```

The external [pyelftools inspector](https://github.com/eliben/pyelftools) reads DWARF source locations. CI installs version 0.33 with its wheel hash pinned; it is build infrastructure and is excluded from project runtime dependencies and release bundles. With that inspector available in a separate audit environment:

```sh
python3 scripts/check_gnu_source_map.py \
  --binary /probe/output/server \
  --source-hashes /probe/output/standard-library-source-hashes.json \
  --dependency-sources /probe/output/dependency-source-hashes.json \
  --output /probe/output/source-line-inventory.json
```

The checker resolves DWARF 4 and 5 file indexes, attributes only nonempty intervals inside executable load ranges, and requires hashes for every mapped standard-library source. Missing debug information, unresolved paths, empty mappings, and missing hashes fail. Known non-allowlisted mapped source paths also fail the check, including generated Unicode even if the link map names no Unicode archive. Mapped instructions from `/usr/include/` fail as unreviewed system-header inputs. This catches header code that an archive-only inventory misses; it does not assign one license to all system headers. The inventory records line-zero and discarded-address intervals separately. Its byte counts can overlap; they are not a coverage percentage. Anonymous constants, unmapped instructions, assembler, generated code, headers without mapped instructions, and complete source-license classification remain outside this check. Every record keeps `licenseClearance: false`.

The dependency snapshot records the resolved package versions, declared licenses, manifest hashes, and license/notice file hashes. Each downloaded `.crate` archive must match its lockfile checksum, and every extracted registry file must match that archive. Extra source files fail. Cargo's own `.cargo-ok` cache marker is excluded from the archive comparison. These are registry archives; [directory sources](https://doc.rust-lang.org/cargo/reference/source-replacement.html#directory-sources) use a different checksum record.

The native compiler wrapper records successful C and assembly inputs before temporary OpenSSL sources are removed. GCC's documented [dependency options](https://gcc.gnu.org/onlinedocs/gcc/Preprocessor-Options.html) supply included headers for preprocessed inputs, including system headers. Plain assembly records its input file. Failed compilations and compiler probes do not establish retained inputs. Multiple hashes for one source path stay ambiguous and fail if that path is mapped into the executable.

With `--dependency-sources`, every mapped dependency path must have a captured hash. The most specific package root supplies its declaration; native and generated files retain a separate review requirement. Byte-identical package files are candidates for provenance review, not an automatic license assignment. Post-build package snapshots and native compiler traces do not establish every compile-time input or individual-file exception. Included-header hashes also do not establish which header content was retained.

Then classify the mapped standard-library paths against the exact compiler's hierarchical notices:

```sh
python3 scripts/check_gnu_notices.py \
  --inventory /probe/output/source-line-inventory.json \
  --source-hashes /probe/output/standard-library-source-hashes.json \
  --notice /probe/output/COPYRIGHT-library.html \
  --output /probe/output/compiler-notice-classification.json
```

The notice checker verifies both input hashes and each mapped source hash, then uses the most specific path rule. The Unicode directory's exception does not override its separately licensed `mod.rs`. License choices follow [SPDX expression precedence](https://spdx.github.io/spdx-spec/v3.0.1/annexes/spdx-license-expressions/): `OR` allows an MIT or Apache-2.0 option, `AND` requires every component to fit, and `WITH` additions remain outside the allowlist. Unknown identifiers do not count as allowed licenses. Malformed expressions, unsupported notice trees, unsafe paths, and changed hashes fail. Vendored crates remain unreviewed until their package/version notices are joined; they cannot inherit Rust's default license.

This classifies the compiler's declared path notices. Individual files may carry additional exceptions, and the source inventory does not cover every instruction or constant. Other dependencies, included headers, external-runtime distribution, and production adoption still need review. CI saves the notice and classification as audit records, excluding server and runtime binaries. A passing notice report retains `licenseClearance: false`.
