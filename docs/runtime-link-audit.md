# Static runtime audit

The Cargo allowlist passes, but the linked runtime has not cleared the requested MIT/Apache-2.0 boundary. This audit identifies inputs for further review; it is not license clearance.

On 2026-10-01, the server was relinked in an existing builder image with networking disabled. The builder contained the socket implementation at `f33c138`; this probe predates catalog filters. The compiler was `rustc 1.98.1 (48a229cea 2026-09-01)`, targeting `x86_64-unknown-linux-musl`.

| Evidence | Identity |
| --- | --- |
| Builder image | `sha256:17f63e4fae492b32ce0f64ae58dc2c87577e7954f55c40fb75ec67d4c0abc52c` |
| Unstripped probe binary | SHA-256 `50ab59dedf6b7835c91351b5895e461baef8a0bba6d4faecba23e50c7901c3de` |
| Link map | SHA-256 `3a9f4b933017b3be75e6f6081f9c57270d31a17dd1efd2130a34621071c0280d` |

The binary, map, linker trace, symbols, ELF headers, archive inventory, and notices are preserved in ignored local storage. The probe exited successfully and retained no dynamic interpreter or dynamic library dependency.

## Inputs found

The linker's archive-inclusion section names 389 selected members of Rust's bundled `self-contained/libc.a` and five members of `self-contained/libunwind.a`. There are 387 distinct libc member names; `free.lo` and `realloc.lo` each occur twice. Count names case-sensitively: `_Exit.lo` and `_exit.lo` are different inputs. Selection does not prove that every section of a member survived garbage collection. Counts use complete archive/member header lines before `Discarded input sections`; cross references and later section entries are excluded.

The startup inputs include `rcrt1.o`, `crti.o`, `crtbeginS.o`, `crtendS.o`, and `crtn.o` from the target's self-contained directory. Rust's [musl toolchain script for 1.98.1](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/src/ci/docker/scripts/musl-toolchain.sh) pins a musl-cross-make revision, musl 1.2.5, and two musl security patches. Its introductory comment lists GCC 9.2.0 and Binutils 2.31.1, but debug metadata in the actual `rcrt1.o` reports GCC 9.4.0; `crti.o` and `crtn.o` report GNU AS 2.44. The comment does not establish the shipped compiler version.

`crtbeginS.o` and `crtendS.o` contain no debug source identity in this probe. Their SHA-256 values are `297960e338581a38bbfcbee45169a847bcdc5527ccaf68abe95b72b3b0856bed` and `0d11009c048ae289cdf184d726b767debc2972321e143838d28d7b6802b69c7c`. Both identify GCC 9.4.0 as their compiler; that does not identify their source license. Rust's [1.98.1 bootstrap](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/src/bootstrap/src/core/build_steps/llvm.rs) builds these objects from LLVM compiler-rt, with initialization and exception-frame registration enabled. Its LLVM submodule is pinned to `52ed14fcd56afc30f9cccd8ca8ce237c2eef7e04`. The pinned [crtbegin source](https://raw.githubusercontent.com/rust-lang/llvm-project/52ed14fcd56afc30f9cccd8ca8ce237c2eef7e04/compiler-rt/lib/builtins/crtbegin.c) and crtend source declare `Apache-2.0 WITH LLVM-exception`.

The map retains startup code, exception frames, and initialization/finalization arrays, including `__do_init` and `__do_fini`, consistent with that bootstrap lineage. An attempted byte comparison could not run because the probe builder lacks the original cross compiler. Exact object reproduction and license treatment remain open. The GCC compiler marker is not evidence that GCC's `crtstuff.c` was linked.

The unstripped server contains `core::unicode::unicode_data` symbols. The compiler's `COPYRIGHT-library.html` assigns Unicode-3.0 to the corresponding data. The current binary therefore cannot be described as meeting the requested boundary.

The selected unwind members include `UnwindLevel1`, register save/restore, and `libunwind` objects. The [license at Rust's pinned LLVM revision](https://raw.githubusercontent.com/rust-lang/llvm-project/52ed14fcd56afc30f9cccd8ca8ce237c2eef7e04/libunwind/LICENSE.TXT) includes Apache terms with LLVM exceptions and a legacy license section. Individual linked files still need provenance and retained-section review. The legacy section has not been treated as blanket MIT clearance for this archive. Compiler builtins need the same file-level review.

## Musl subset review

The official [musl 1.2.5 archive](https://musl.libc.org/releases/musl-1.2.5.tar.gz) was downloaded for its notices. Its SHA-256 is `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4`. No implementation was copied into Puffinbox.

An initial review checked 13 math members. A broader name-based inventory then matched 385 of the 387 distinct selected member names to source candidates, preferring x86-64 files over generic names. Six candidates have explicit leading notices: `__set_thread_area`, `__unmapself`, `exp_data`, `pow`, `pow_data`, and `qsort`; all six identify MIT. Debug metadata identifies the two previously unmatched names, `malloc` and `aligned_alloc`, as files under `src/malloc/mallocng`, built with GCC 9.4.0.

The duplicate allocator members were extracted separately with `ar xN`, preserving each occurrence. Debug metadata identifies the first `free.lo` as `src/malloc/free.c` and the second as `src/malloc/mallocng/free.c`; the first `realloc.lo` is `src/malloc/mallocng/realloc.c` and the second is `src/malloc/realloc.c`. All four identify GCC 9.4.0. Object hashes and metadata are preserved with the audit.

Separate extraction of all 389 selected libc objects then found source-path metadata for 378. Every identified path exists in the official musl 1.2.5 archive; the join records candidate source hashes beside object hashes. Eleven objects lack that source-path metadata. These path matches improve the inventory without proving that the shipped objects were built from those exact source bytes.

This inventory is not complete license clearance. Source-path metadata does not establish exact source bytes, and leading comments do not account for included headers, patches, or every retained section. These gaps remain open.

The builder's Debian musl copyright file and Rust's bundled libc are separate evidence. A notice for the builder package does not identify the target archive's version or clear its linked subset.

## Panic-abort probe

A separate build used `CARGO_PROFILE_RELEASE_PANIC=abort` in the same isolated builder. It succeeded with binary SHA-256 `73d023b8be6cf52ae447f658a0cc2f4ff703d2198b706235d79294e098298936`. Its map still selects 389 libc members and five unwind members; its symbols still include core Unicode tables and unwind functions. Changing this profile alone does not remove the identified blockers. No production panic setting was changed. Cargo also [ignores this setting for ordinary tests](https://doc.rust-lang.org/cargo/reference/profiles.html#panic), so passing the standard suite would not validate an aborting production build.

An isolated follow-up rebuilt the standard library from Rust 1.98.1 sources with an empty standard-library feature set and `panic=immediate-abort`. These are [experimental Cargo options](https://doc.rust-lang.org/cargo/reference/unstable.html#build-std); the probe used `RUSTC_BOOTSTRAP=1`, retained the original project lockfile, and did not change the production toolchain or profile. Its builder was `sha256:26dc10cfb31747d4d9f5f43a73f16ca95c2bb426d7d3669fa0799362b7cb38c8`, derived from the same `f33c138` builder with `rust-src` added. Binary SHA-256 was `2244eea7557fe9f3bb37984595bb5376b1660f671167f6572fa340935d127c3a`.

That map selects 387 libc members and five unwind members. The unwind contributions that remain in the output map are three compiler-identification `.comment` entries; no `_Unwind_` functions appear in the binary symbols. By comparison, the baseline and profile-only abort maps retain executable unwind sections. The rebuilt probe still has 39 core Unicode-data symbols and the existing startup objects, so it does not clear the runtime boundary. It is a link-input experiment, not a validated alternative runtime for the server. Its evidence is preserved under `.local/runtime-build-std-20261002`.

## Dynamic-link probe

An isolated current-source build tested Rust's [prefer-dynamic option](https://doc.rust-lang.org/rustc/codegen-options/index.html#prefer-dynamic) for `x86_64-unknown-linux-gnu`. The source was mounted read-only at the dirty `1bf5b2a` working tree, with networking disabled and the project lockfile retained. Binary SHA-256 was `3b48358ac1a68db0b2be02c34680d10c884124239ef802f1f9efdba242bbd413`.

Its symbol inventory imports seven core Unicode-data symbols and defines a whitespace lookup function. The whitespace table is imported from the shared standard library. It also imports `_Unwind_Resume`. Required shared objects are `libstd-64f5f36fb0927694.so`, `libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, and the GNU loader. The standard-library input has SHA-256 `2988c07760fd0baeab583e75dc23aa56a56c5d169d94bcaeceb1c933318e32bb`.

The output map still retains system `Scrt1.o`, `crti.o`, `crtbeginS.o`, `crtendS.o`, and `crtn.o`. This changes where runtime code and data reside without establishing an allowed distribution: retained startup objects, the Unicode lookup body, and any proposed runtime bundle need their own license review. The probe has no server acceptance result, and production remains the static musl build. Evidence is preserved under `.local/runtime-dynamic-retry-20261002`; the earlier failed helper invocation is retained separately.

## Original GNU entry and wrapper probe

A follow-up at clean source `e72cfbd` replaced the five GNU startup inputs with an original process entry. Binary SHA-256 was `8262a6a4abe8dcd3c7e773f33a42dae7e0aa4f89e29e2c48aec9129c156ba242`. C and Rust lifecycle fixtures passed, and the server rejected an invalid configuration with exit code one. Its map still retained `libc_nonshared.a(atexit.oS)` and `libc_nonshared.a(pthread_atfork.oS)`.

Original bridges to the public LSB exit- and fork-registration interfaces then replaced those two wrappers. The first server link failed because the libc definition was selected before the bridge object. A fresh link used `--wrap=atexit,--wrap=pthread_atfork`, producing binary SHA-256 `d13f71aa00f9550e54a449e8270ef645558c3b146192367c5f6b6523176a106e`. No system startup or libc-nonshared sections remain in that output map. The LLD cross-reference table still lists the lazy archive members; those lines do not establish retention.

The [original sources and reproduction steps](../experiments/linux-gnu-entry/README.md) are retained in the repository. Their fixtures check initialization and cleanup order, arguments and environment, thread-local values, exit status, fork callback order, and Rust panic catching. The standalone checker also passed on the WSL host and [in the successful `69e26db` cloud run](https://github.com/peppermintish/puffinbox/actions/runs/36946522847).

The wrapped binary passed 22 server media/API checks with one pending restart check in a fresh database and copied fixture tree. It ran as UID/GID 10001, with a read-only root, dropped capabilities, and external standard-library and FFmpeg runtimes. It exited with code zero and logged that media children had drained. The final runtime ledger is preserved under `.local/runtime-original-bridges-acceptance-final-20261002`; link evidence is under `.local/runtime-original-bridges-retry-20261002`. Failed setup, link, startup-race, and fixture-path attempts remain preserved separately.

This binary still imports seven core Unicode-data symbols and defines a generated whitespace lookup. It retains compiler-builtins sections and needs the same external Rust standard library and GNU libraries as the earlier dynamic probe. Exact source and retained-section review, runtime distribution, restart/resume, and broader client acceptance remain open. The production static build and release gates are unchanged.

## Shared standard library and external compiler helpers

A subsequent isolated build from clean source `69e26db` rebuilt Rust 1.98.1's standard library as both an archive and a shared library. In the disposable builder, 13 `inline` or `inline(always)` attributes in generated `unicode_data.rs` were changed to `inline(never)`; eight such attributes were already present. The source SHA-256 changed from `d3d218b7574f08efe423e9d4b6b539e700046aca4f2a2793d38164ec5a1b00b2` to `c704a0bde2b54990bac3009ba9c7db5876097012dd9f52dfc1bf4add9ef38c96`. Tables were unchanged. The resulting executable imported seven generated Unicode lookup functions and defined none of those symbols.

That intermediate binary, SHA-256 `b090c4586a3ceb4f2877f89daaff95054215e78a12d66f8d288cac6262a1b64c`, still retained 36 compiler-builtins sections from six code-generation objects, including 11 executable functions. An initial inventory filtered only `/usr` inputs and omitted these rebuilt archives under `/cache`; the corrected inventory covers both. Compiler-builtins' root license is `MIT AND Apache-2.0 WITH LLVM-exception`, outside this allowlist. Its MIT libm subdirectory does not clear the complete retained crate.

The final executable link excluded that one compiler-builtins archive. The nine retained math helpers now import from `libm.so.6` at `GLIBC_2.2.5`; `__floattidf` and `__umodti3` import from `libgcc_s.so.1` at `GCC_3.0`. The shared standard-library producer still includes compiler-builtins, entirely in the external runtime. The server SHA-256 is `38516f9e61bb5d6c21b049df6a4ef41e0c32cd90ba7ab678247561293b62d44c`; its shared `libstd-3811eecf5c907fe6.so` has SHA-256 `ee59e176c0ef7eb9682b8c6b893cefd81b0ffbcad8d44eca2508a97a75ea2a57`.

The LLD output map has no retained sections from the known standard-library/runtime archives, system startup objects, or libc-nonshared wrappers. Lazy archive names in the cross-reference table are not retained sections. The map and symbol checker rejects unsupported or empty maps, and its regressions cover rebuilt archives outside `/usr`, lazy references, generated Unicode bodies, and startup metadata. These observations do not establish complete license clearance for inlined code or every bundled input.

A differential fixture compared all 11 helper functions, including signed zero, subnormals, halfway rounding, infinities, NaNs, and 4,096 deterministic integer and floating inputs. Outputs matched a baseline retaining compiler-builtins. Python integer arithmetic and conversion independently checked 128-bit modulo and signed-to-double results. Unicode behavior also passed. The exact server then passed 25 container checks, including active FFmpeg shutdown, socket reconnection, and playback resume, plus 29 isolated HTTPS proxy checks. Both services exited with code zero; the owned containers were removed and persistence volumes preserved.

The [public reproduction](../experiments/linux-gnu-runtime/README.md) retains the original wrappers and fixture. It uses experimental Cargo options in a disposable builder, preserves Cargo's jobserver descriptors, and records source, executable, and runtime hashes. The source remains mounted read-only and the project lockfile is unchanged. Evidence lives under `.local/runtime-unicode-shared-retry-20261002` and the four `runtime-external-*` probe directories named in acceptance.md. The initial missing-manifest-context replay and failed setup attempts remain preserved separately.

Production still uses static musl. This GNU candidate needs exact inlined-source provenance, a distribution design that keeps the non-allowlisted shared runtime external, broader runtime and client checks, and deliberate production adoption. No release gate has been marked passed.

The public recipe also passed from a fresh disk-backed output directory on 2026-10-02, producing server SHA-256 `be3f42165eb98c9b520c17b9d7e5305b3fe52c21be91c71a98c89be2c5cea165` and the same external standard-library hash. Its map inventory and 4,096-row numeric differential passed, with all 11 baseline definitions and alternative imports verified and no Cargo jobserver descriptor warnings. The [experimental `a2dee43` cloud workflow also passed](https://github.com/peppermintish/puffinbox/actions/runs/36953389444). Records are under `.local/runtime-reproduction-disk-20261002`. Earlier recipe failures remain separate: an attribute-count guard, the direct fixture's duplicate core dependency, and exhausted RAM-backed `/tmp`. Generated compilation caches were preserved on disk, with relocation records; fixtures and test ledgers were retained.

The exact `be3f4216` binary subsequently passed 25 container checks and 29 native-Linux HTTPS proxy checks. The first HTTPS setup failed because the recipe's final `copyfile` dropped the server's executable permission, producing mode 0644. The Windows-mounted container check masked that permission bug. The recipe now sets mode 0555 and requires a Linux startup check to reach the expected missing-database error with exit code one. A separately preserved copy with the corrected permission passed startup and HTTPS checks; its byte hash is unchanged. Evidence is under `.local/runtime-reproduction-acceptance-20261002`, `.local/runtime-reproduction-executable-20261002`, and `.local/runtime-reproduction-https-executable-20261002`; the failed permission check remains in its original directory. [The experimental cloud workflow at `b26c91e` passed the startup assertion](https://github.com/peppermintish/puffinbox/actions/runs/36955711054).

## Instrumented source locations

A separate build from clean source `6560147` retained release debug data without changing production. Server SHA-256 is `96e26285e00ef4e722d407852f182dab3a1b1c7d509969ffaf880a130b35cecb`; external standard-library SHA-256 is `5a3f35a0e2e734369effc845ebad8b182912c600ad2114ce400ee3d2502b1ea0`. Startup, link inventory, and all 4,096 numerical comparisons passed. The debug build's external-library hash differs from the preceding optimized-only probe; its runtime must be identified separately.

The source-location inventory examined 3,591 compilation units. It attributed 1,463,499 nonempty instruction intervals inside executable load ranges to 2,427 source files, including 245 standard-library files. Every mapped standard-library path joined to an exact source hash captured in the disposable builder. No mapped generated Unicode or compiler-builtins path was found. The checker also records 185,888 line-zero intervals and 426,318 discarded or invalid-address intervals. These counts are line-table observations, not byte coverage or a percentage of the executable.

The compiler's complete installed notices were retained with the local evidence. Its standard-library notice assigns MIT OR Apache-2.0 by default, with exceptions for generated Unicode and a Fuchsia mutex file, among others. The mapped-source inventory checks known exceptions; it does not replace individual-source review. Anonymous constants, instructions without source locations, assembler, generated code, included headers, and external-runtime distribution remain open. No production runtime or release gate has changed.

The [public recipe](../experiments/linux-gnu-runtime/README.md#source-location-inventory) now accepts `--source-map` and captures exact source hashes. The separate [checker](../scripts/check_gnu_source_map.py) uses an external DWARF inspector and rejects missing data, unresolved file indexes, and missing standard-library hashes. Eight regressions cover DWARF 4/5 indexes, sequence boundaries, discarded addresses, absent hashes, and mapped Unicode independent of archive names. All 40 Python tests passed locally. Evidence is preserved under `.local/runtime-source-map-20261002`.

A fresh replay of that public flag passed startup, link inventory, numerical comparisons, source capture, and the public source checker. Its server SHA-256 is `1f09d8f175795cf6c8ae8e12133d8dd6a0c69083e39378c4470e0966758d995d`; the external standard-library hash is unchanged. It maps the same 245 exact standard-library source files without known non-allowlisted mapped paths. This is a separate executable and has no server acceptance or native-player result. Its record is under `.local/runtime-source-map-recipe-20261002`.

## Mapped GNU header instructions

A follow-up review found instruction intervals attributed to `/usr/include/stdlib.h` and `bits/byteswap.h` in the preceding `1f09d8f1` candidate. The archive inventory did not identify these header contributions. The strengthened source checker reports every mapped `/usr/include/` input as unreviewed and rejects the old candidate. Its preserved rejection is under `.local/gnu-header-check-before-20261002`.

The experimental C recipe now disables optional GNU header inlining and uses an original adapter built from documented GCC byte-swap operations. Public system headers supply types and declarations; byte-order conversions use fixed-width casts and evaluate arguments once. An external `atoi` call replaces the optional inline body. The adapter fixture passed 4,096 deterministic comparisons against both the ordinary C baseline and an independent byte oracle. GNU datagram declarations and preprocessed assembly have separate fixture coverage. Earlier full-build failures exposed feature-macro ordering and assembly inclusion; their logs remain under `.local/runtime-c-headers-20261002` and `.local/runtime-c-headers-20261002b`.

The corrected full replay produced server SHA-256 `171be7a3b197f0eca71be8a13c2b82c42e4dfa755e90cf9f2470af1df505ba9f`, with external standard-library SHA-256 `5a3f35a0e2e734369effc845ebad8b182912c600ad2114ce400ee3d2502b1ea0`. It was built from the dirty `5307cf0` tree with the playback event fix later committed as `4c09ba3`; exact source hashes are recorded. Startup, link inventory, all 11 helper providers, 4,096 numerical comparisons, the C fixture, and the strengthened source checker passed. The inventory resolves 2,425 source files, including 245 standard-library files with exact hashes, and reports no known non-allowlisted standard-library paths or mapped system-header instructions. Its 1,468,095 mapped intervals, 186,870 line-zero intervals, and 426,690 discarded or invalid-address intervals are observations, not a coverage percentage.

That exact executable passed 26 container checks, including active FFmpeg shutdown and persisted resume across restart, plus 29 isolated HTTPS checks. The official Qt 6 desktop then decoded both synthetic FLAC tracks through EOF and advanced automatically against the same executable. All 19 playback responses returned 204, including duplicate stops; an independent read confirmed retained completion. Its immutable native record is `.local/desktop-qt6-20261002/native-private-playlist-gnu-c-headers-results.json`. Listening and broader client behavior remain unvalidated. Its native Linux executable mode is 0555. The operator image and shared library were external test inputs; the production static server was not replaced. Evidence is under `.local/runtime-c-headers-20261002c`, `.local/runtime-c-headers-acceptance-20261002`, and `.local/runtime-c-headers-https-20261002`. All 41 Python regressions passed, including the new mapped-header case. Individual source exceptions, anonymous constants, unmapped instructions, assembler, generated code, other included headers, and external-runtime distribution still require review. `licenseClearance` remains false.

## Exact compiler notice classification

The source-map recipe now captures the same compiler's `COPYRIGHT-library.html` and records its hash alongside the source hashes. The [notice checker](../scripts/check_gnu_notices.py) verifies that chain and joins each mapped standard-library path to the most specific hierarchical rule. It preserves the separately licensed Unicode module exception and rejects required licenses outside MIT/Apache-2.0. Vendored crates remain unreviewed instead of inheriting Rust's default license. Its expression evaluator respects SPDX choices, required components, additions, and parentheses; malformed inputs fail even after an allowed alternative.

Ten new regressions cover notice nesting and boundaries, Unicode and Fuchsia exceptions, license-expression precedence, vendored paths, unsafe and duplicate paths, changed notices, mismatched source records, and immutable rejection output. All 51 Python tests passed, and the checker parsed all six hierarchical rules in the installed Rust 1.98.1 notice. Source hashes and the exact notice hash are recorded under `.local/gnu-notice-source-checks-20261002`.

The fresh recipe replay from `ef6e79f` produced server SHA-256 `f5d16873b204ab557406daaafcde8f0bf703c20a1bbd2c953b0e3264ca054b74` and the unchanged external standard-library SHA-256 `5a3f35a0e2e734369effc845ebad8b182912c600ad2114ce400ee3d2502b1ea0`. Startup, link inventory, both 4,096-row fixtures, source mapping, and notice classification passed. All 245 mapped standard-library files fit the applicable declared notices. The inventory reports 2,425 source files, 1,468,101 mapped intervals, 186,874 line-zero intervals, 426,690 discarded or invalid-address intervals, and no known non-allowlisted mapped standard-library paths or system-header instructions. These counts do not measure complete coverage.

That exact executable passed 26 container checks, including active FFmpeg shutdown and persisted resume across restart, plus 29 isolated HTTPS checks and a private-playlist catalog supplement. Owned test containers were removed and persistence volumes retained. It has no native-player result; earlier native observations belong to their separately identified executables. Evidence is under `.local/runtime-notice-recipe-20261002`, `.local/runtime-notice-acceptance-20261002`, and `.local/runtime-notice-https-20261002`. The captured compiler notice has SHA-256 `68129500b616d5838629e68f55ff3aed5e096dacf60ce9eb41bbe599a563afa6`. The experimental CI workflow now repeats the notice check and preserves its input notice and classification, excluding executable and runtime binaries.

This is classification of declared path notices. Individual-file exceptions, anonymous constants, instructions without source locations, assembler, generated code, other dependencies and headers, external-runtime distribution, and production adoption still require review. Every report retains `licenseClearance: false`.

A registry-file follow-up on GNU `d7d42ac4` verified 1,136 mapped registry source files against 138 exact locked archives. Each mapped file matched its captured hash. The separate 66 local package inputs and 927 native inputs retain their existing scopes. Leading notice and generator candidates are preserved under `.local/mapped-package-notices-20261003`; the first registry-only helper stopped on a native mapping without a package identifier and has a separate failed record. Three BSD keyword candidates were platform references rather than license notices. Generated tables in `unicode-width`, `unicode-normalization` and `unicase` remain provenance review candidates; accepted package declarations and code headers do not finish their input or retained-data review. This follow-up classifies no new license and leaves full-runtime clearance false.

## Dependency and native compiler source snapshots

The GNU recipe now checks each cached registry archive against the exact project's lockfile checksum, then compares every extracted file with that archive. Missing, modified, additional, linked, or unsafe archive inputs fail. Cargo's root `.cargo-ok` marker is cache metadata and is excluded from the archive comparison. Package records preserve versions, declarations, manifest hashes, and notice hashes. Local workspace and vendor inputs remain separately identified.

OpenSSL removes temporary build sources before the recipe finishes. The [native compiler wrapper](../experiments/linux-gnu-runtime/native-source-wrapper.py) records successful compile inputs before returning, with GCC dependency rules for preprocessed inputs and their headers. Plain assembly records its source file. It preserves available jobserver descriptors and forwards the original compiler arguments, adding dependency output for the audit. The snapshot preserves changed-source ambiguity rather than selecting an arbitrary hash. A mapped path with conflicting or missing hashes fails. These traces include compiler probes and non-retained inputs; their counts do not measure executable coverage.

The fresh replay from `8466389` produced server SHA-256 `65f20a63dbdd745885d42991c7b2f073c69837f7386d8872f442c10ba7c2f5ae`, with unchanged external standard-library SHA-256 `5a3f35a0e2e734369effc845ebad8b182912c600ad2114ce400ee3d2502b1ea0`. Startup, numerical and C fixtures, link/source inventories, and the exact compiler-notice classifier passed. All 2,422 mapped source paths have hashes: 245 standard-library paths, 1,194 package paths, and 983 native compiler paths. The recipe verified 220 registry archives within 226 package records. It captured 13,142 available package files, 147 remaining generated files, and 1,747 distinct native inputs from 1,146 successful compile invocations, with no ambiguous native paths. No known non-allowlisted mapped standard-library path or mapped system-header instruction was reported.

Of the 983 mapped native inputs, 875 have byte-identical package-file candidates. The remaining 108 require generated or changed-input provenance review. Byte equality does not assign a license, and package declarations do not clear individual-file exceptions. Included-header hashes do not identify which header content was retained. Anonymous constants, unmapped instructions, assembler includes, complete source exceptions, external-runtime distribution, and production adoption remain open.

That exact executable passed all 27 container checks, including playlist sharing, active FFmpeg shutdown, and persisted resume across restart, plus a private-playlist catalog supplement and all 29 isolated HTTPS checks. It has no native-player result. Fifteen source-capture regressions bring the Python suite to 66 passing cases, including a real C compilation whose header remains recorded after source cleanup. Evidence is under `.local/gnu-dependency-sources-20261003b`, `.local/gnu-dependency-container-20261002`, and `.local/gnu-dependency-https-20261002`. The dependency snapshot has SHA-256 `f49fef5c27e02261b7ff049c5080cb69e4178872468c498b2718175d59769f47`. CI retains source, notice, and compiler-trace records, excluding executables and external runtime libraries. Every report keeps `licenseClearance: false`; production remains the static musl image.

## Preserved native source copies

Earlier compiler traces kept hashes after OpenSSL removed its temporary sources, but they did not keep the source bodies. The recipe now preserves complete, hash-addressed copies before returning to the build script. GCC reports preprocessor headers; GNU assembler dependency rules also report `.include` and `.incbin` inputs. The wrapper checks that each file remains unchanged while it is copied, publishes each copy atomically, and records both the original and actual compiler arguments. These are post-compilation copies, so they cannot establish which bytes were compiled if a file changed during compilation.

New recipe builds require the copies. The source inspector can verify a relocated byte directory against the exact source index and rejects missing, modified or linked copies and escaping or incomplete indexes. Historical hash-only traces remain readable and explicitly lack complete byte-copy verification. The copies can contain non-allowlisted source and system headers; they stay in private audit output and are excluded from CI uploads and release bundles. Hash equality and textual notice hints do not assign a license.

All 100 Python cases passed, including concurrent real C compilations, source cleanup, copy integrity, historical-record rejection, relocation, and real plain/preprocessed assembly with included source and binary constants. The first fresh GNU recipe passed its internal checks, but host inspection failed because copies were readable only by the builder's account. That failure is preserved under `.local/native-source-bytes-20261003`. Copy permissions were corrected for host review within the private output directory. A second build failed when an assembler dependency used a debug basename without its source directory; that record is under `.local/native-source-bytes-20261003b`. Missing labels now resolve only to one exact suffix match among known compiler inputs, with recorded aliases and rejection of unknown or ambiguous matches. A real nested-source compilation and ambiguity controls cover the repair. Whole-runtime clearance remains false.

The fresh repaired recipe, from the dirty tree at `fc2763e`, produced server SHA-256 `d7d42ac49d7ae902a6a460a10c7304e5422292f6ff9e287cfe7f8598a12e694d`. Its external standard-library hash remains `5a3f35a0e2e734369effc845ebad8b182912c600ad2114ce400ee3d2502b1ea0`. Tracked source fingerprints remained unchanged throughout the build. Startup, both numerical fixtures, outbound TLS, OpenSSL exclusions, link/source inventories and compiler-notice classification passed. The executable matches the first recipe's bytes; the capture repair did not change this executable.

Host-side verification checked 1,682 distinct copies totaling 24,069,116 bytes, representing 1,683 source paths from 1,101 successful native compilations. No native path had conflicting hashes. All 927 mapped native source bodies remain available by verified hash even though all 927 originals were removed. The full inventory has 2,374 mapped paths: 245 standard-library, 1,202 package and 927 native compiler paths. No known non-allowlisted mapped standard-library path or mapped system-header instruction was reported. Of the native paths, 820 have byte-identical package-file candidates and 107 lacked such candidates. Textual notice hints remain candidates for review, not license assignments.

That exact executable passed all 33 container checks, including automatic embedded metadata import and persistence, active FFmpeg shutdown and saved video progress across restart, plus a playlist catalog supplement and all 29 isolated HTTPS checks. These fixtures mounted its binary and external standard library on separately owned operator infrastructure; production packaging is unchanged. The owned test containers were removed, with container persistence volumes retained. There is no official native-player result for this GNU executable. Evidence is under `.local/native-source-bytes-20261003c`, `.local/native-source-bytes-container-20261003` and `.local/native-source-bytes-https-20261003`. The dependency snapshot has SHA-256 `f3f8139efd297848e2a68e71c41818c6b93833c78a6ce91d7d2af37e45474a32`. Every report keeps `licenseClearance: false`; individual-file and generated-input provenance, unmapped content, external-runtime distribution and production adoption remain open.

A separate replay regenerated all 107 unmatched bodies byte-for-byte from the locked `openssl-src 300.6.1+3.6.3` archive, SHA-256 `46eb8fb9fb3b61ce1c0f8a026c4c1a0714d3a9e138e7fbde78753ce2babc3846`. It verified all 2,428 extracted OpenSSL files against the captured source hashes, replayed the recorded Configure arguments through the current wrapper, and generated only the selected targets with the same pinned builder. The evidence retains Configure arguments, generated configuration hashes and each expected/actual output hash under `.local/openssl-regeneration-20261003`. These are generated files absent from the original archive; they are not unexplained package edits.

The recorded build commands bind each output to its exact generator or template. Of those 107 selected inputs, 106 have explicit Apache-2.0 headers. The remaining Intel AES-XTS generator refers to OpenSSL's distribution license, whose captured `LICENSE.txt` is Apache-2.0 (SHA-256 `7d5450cb2d142651b8afa315b5f238efc805dad827d91ba367d8516bc9d49e7a`). OpenSSL's [license page](https://openssl-library.org/source/license/) identifies Apache-2.0 for releases from 3.0 onward. Several generators retain CRYPTOGAMS origin notes or public-domain references, as does the byte-identical ARIA source. Their exact notice contexts remain in the private review. Output equality establishes this generation provenance; header presence and the release-level statement do not replace file-level review or clear unmapped inputs. No license policy, dependency source or runtime distribution changed.

The mapped native declaration follow-up verified all 927 preserved source bodies against the locked archive or the 107 already reproduced generated outputs. Each source or selected generator declares an Apache-2.0 option, including Intel's reference to the captured OpenSSL distribution license. The full-file notice search found explicit dual-license alternatives in [LPdir_unix.c](https://raw.githubusercontent.com/openssl/openssl/openssl-3.6.3/crypto/LPdir_unix.c), [v3_pci.c](https://raw.githubusercontent.com/openssl/openssl/openssl-3.6.3/crypto/x509/v3_pci.c) and [v3_pcia.c](https://raw.githubusercontent.com/openssl/openssl/openssl-3.6.3/crypto/x509/v3_pcia.c); their BSD alternatives are choices, not additional required terms. Twenty generated assembly inputs identify a CRYPTOGAMS distribution condition in their exact OpenSSL-origin generators, such as [aes-x86_64.pl](https://raw.githubusercontent.com/openssl/openssl/openssl-3.6.3/crypto/aes/asm/aes-x86_64.pl). Three other BSD text matches describe operating systems. Exact hashes, source/generator joins and notice contexts are under `.local/openssl-file-notices-20261003`. The public-domain origin notes in ARIA and two AES generators, included headers, unmapped inputs and complete distribution review remain separate; this record does not clear the runtime or change source or policy.

## External OpenSSL experiment

The optional `--external-openssl` recipe uses OpenSSL 3 shared libraries instead of retaining its vendored native code. The fresh replay from the dirty tree at `574927c` produced server SHA-256 `10efeb22604c48544e94820b4cc4c7ff767a1e5d75f87cb1ef93929f207c363d`, with the unchanged external standard-library hash. The builder reported OpenSSL 3.0.20. The executable declares `libssl.so.3` and `libcrypto.so.3`, has 60 native TLS imports, and has no named static TLS archive inputs or defined native TLS symbols. Startup, link/source inventories, compiler notices, and both 4,096-row fixtures passed.

All 1,439 mapped source paths have hashes: 245 standard-library and 1,194 package paths, with no mapped native compiler paths or system-header instructions. The source snapshot has SHA-256 `f0cb2527c0c439077b9dea21884022ea0e38cc69d23267999dade1dbce1686b9`; it records 13,144 package files and five remaining generated files. Eight native compiler trace records include two successful probe compilations and 65 inputs. Those probes do not establish retained inputs. The smaller inventory reduces vendored native provenance work; individual-file exceptions, unmapped content, and complete packaging review remain open.

That exact server passed 27 container checks, a catalog supplement, and 29 isolated HTTPS checks. The official Qt 6 desktop resumed the retained 123.185588-second position after server container recreation, first decoded at 123.2 seconds, displayed advancing video, and accepted all 28 playback responses including stop. An independent read confirmed 272.866 seconds saved with `Played=false`. The retained server identity also matched. Evidence is under `.local/gnu-shared-tls-20261003`, `.local/gnu-shared-tls-container-20261003`, `.local/gnu-shared-tls-https-20261003`, and `.local/gnu-shared-tls-native-20261003`. The original post-stop screenshot was occluded; a separately preserved, freshly activated observation confirms Home. Request, decoder, and saved-position evidence independently establish the stop.

The public TLS helper check passed all four certificate cases against the existing external and vendored server dependency artifacts. Evidence is under `.local/gnu-tls-recipe-check-20261003`; the helper addition was tested separately after those server builds and did not change their bytes. An earlier helper attempt omitted a host dependency directory and failed to resolve the procedural macro; its failed build log remains under `.local/gnu-shared-tls-outbound-20261003`. Forwarding the captured host and target dependency paths fixed that build. CI now repeats the full recipe in both TLS modes and saves check records without binaries, certificates, or private keys. Six new inventory and capture regressions bring the Python suite to 72 passing cases.

This is still an experiment. The external standard library contains non-allowlisted components and is excluded from permissive-only release bundles. External-runtime distribution and production adoption remain unresolved. Every report keeps `licenseClearance: false`; the production Dockerfile is unchanged.

## Mapped file exception preflight

A read-only follow-up on 2026-10-03 checked all 245 mapped standard-library files from server `10efeb22` against the official Rust 1.98.1 `rust-src` component. Every file hash matches the captured build input. A textual search for copyright, license and source-attribution hints found the retained `core/src/slice/memchr.rs` copyright and the `core/src/str/pattern.rs` reference to an earlier memchr implementation, alongside ordinary code documentation. No additional SPDX license declaration was found in those mapped files. The record is `.local/gnu-standard-file-preflight-20261003.json`; it retains exact file hashes and the earlier classification hash.

A separate diagnostic searched 8,634 source files in the currently available locked GNU Cargo packages. Its 20 license hints are zerocopy files declaring `BSD-2-Clause OR Apache-2.0 OR MIT`, which offers an allowed alternative under the existing policy. Evidence is `.local/gnu-file-license-preflight-20261003.json`. This scan is not tied to every retained input of the earlier executable and excludes non-source files.

The mapped Rust `pattern.rs` points to memchr revision `8037d11b4357b0f07be2bb66dc2659d9cf28ad32` for one adapted function. That exact revision declares [Unlicense OR MIT](https://raw.githubusercontent.com/BurntSushi/memchr/8037d11b4357b0f07be2bb66dc2659d9cf28ad32/Cargo.toml) and supplies an [MIT notice](https://raw.githubusercontent.com/BurntSushi/memchr/8037d11b4357b0f07be2bb66dc2659d9cf28ad32/LICENSE-MIT), so an allowed MIT choice is available for that cited adaptation. The notice, package declaration and hashes are preserved under `.local/gnu-memchr-adaptation-20261003`. This narrow result does not identify the historical revision behind `slice/memchr.rs` or clear the remaining runtime inputs.

Rust 1.98.1's official [REUSE annotations](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/REUSE.toml) and [cached file-license metadata](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/license-metadata.json) declare an MIT or Apache-2.0 option for all 245 hash-matched mapped standard-library source paths in the `10efeb22` experiment. Both records were checked; negative controls reject the Unicode data and the Fuchsia mutex exception while accepting the separately licensed Unicode module wrapper. Input hashes, per-file declarations, copyright notices and controls are under `.local/gnu-rust-reuse-preflight-20261003`. This closes the declaration lookup for those mapped files, including `slice/memchr.rs`; it does not identify every retained constant, inlined or unmapped input, clear the external standard library, or authorize production adoption. Whole-runtime clearance remains false.

These searches help identify files for review. They do not establish all included-header licenses, unmapped code or constant provenance, the external runtime's distribution boundary, or whole-release compliance. Both records keep `licenseClearance: false`, and production remains the static musl image.

## Reproducing the link inventory

Inside the locked Linux builder, relink with a fresh output directory:

```sh
CARGO_BUILD_JOBS=2 cargo rustc --locked --release \
  --target x86_64-unknown-linux-musl --bin puffinbox-server -- \
  -C link-arg=-Wl,-Map=/audit-output/server-link.map,--cref,--trace
nm -C target/x86_64-unknown-linux-musl/release/puffinbox-server
readelf -h -l -d target/x86_64-unknown-linux-musl/release/puffinbox-server
```

Keep the compiler's notices and the exact link inputs with the results. Changes to the compiler, target, dependency graph, link settings, or application require a fresh inventory. Release packaging stays blocked until the complete boundary passes.

## Mapped package file exception

A separate byte comparison matched all 1,194 mapped package paths of GNU server `10efeb22` against verified registry archives or its recorded project revision. A diagnostic found notice hints in 89 files; that search does not classify every file license or clear unmapped content. The exact futures-channel `src/mpsc/queue.rs` has a two-clause BSD-style notice with no stated MIT/Apache alternative, despite its allowed package expression. Its hash and preserved notice are under `.local/gnu-mapped-package-preflight-20261003/queue-notice.json`. The [licensing inventory](licensing.md) records the original queue replacement and mandatory selected-source guard. Earlier graph checks and mapped hash coverage did not clear this exception. Whole-runtime license clearance remains false.

A fresh external-TLS GNU replay produced server `2df83ea9a7fcf3ac7806d2b78687b56d0f12f6bbfebcebe1c7e8352124775c5e`. The selected local queue source hash is `398f3d9f79982b8f0c1458a4c4def36a58119521fdb2f18c5e2976f91f66e478`; it differs from the excluded upstream file and matches the mandatory replacement review. The capture and five mapped channel paths identify the local package, with no mapped registry futures-channel inputs. Build, startup, numerical, outbound TLS, source inventory and compiler-notice checks passed. The candidate also passed all 31 container and 29 HTTPS checks on separate test infrastructure. The records are `.local/channel-gnu-runtime-20261003/channel-source.json`, `.local/channel-gnu-container-20261003` and `.local/channel-gnu-https-20261003`. It has no official native-player result. These observations repair the known queue exception and validate this candidate's exercised paths; other file exceptions, inlined or unmapped content, the external standard library and distribution remain open.

## System headers in dependency rules

A follow-up found that OpenSSL supplies `-MMD` to its compiler calls.
Appending `-MD` did not recover system headers in the recorded GCC driver.
The [GCC dependency options](https://gcc.gnu.org/onlinedocs/gcc/Preprocessor-Options.html)
distinguish user-only rules from complete rules. A real compilation of an
original `stdint.h` fixture reproduced the omission: the old wrapper
preserved one input, while the corrected wrapper preserved twenty, with
byte-identical object output. That isolated record is under
`.local/native-mmd-header-probe-20261004`.

The wrapper now replaces inherited user-only driver options and removes
explicit preprocessor dependency destinations before selecting its complete
rule. Other preprocessor options remain intact, and the record retains both
original and actual arguments. All 101 Python tests passed, including real
compiler regressions for `-MMD`, its long alias and direct preprocessor
options, source cleanup, system-header byte preservation and unchanged
object output. An initial direct-preprocessor attempt still redirected the
rule to the caller's destination; the regression exposed the missing audit
rule, and the final repair covers that case.

The corrected full build, from the dirty tree at `efcaf03`, produced server
SHA-256 `d1275bc2181ba126d1b834c3d43bea5082bddc3dc620b4395db287737173dc28`.
Its 318 tracked source fingerprints remained unchanged during the build.
Startup, numerical, TLS, OpenSSL exclusion, source inventory and compiler
notice checks passed. Host verification checked 1,872 distinct preserved
files totaling 25,071,775 bytes, covering 1,873 native paths from 1,101
successful compilations without conflicting hashes. These include 252
system paths. All 927 mapped native bodies match the preceding candidate's
exact hashes, retaining the earlier locked-archive and generator evidence.
The full mapped inventory contains 246 standard-library, 1,204 package and
927 native paths.

A conservative dependency review joined successful compilations reporting
any mapped native input. It covers all 927 mapped bodies and 1,841 total
inputs, including 248 system paths. Shared mapped headers can include unused
compilation units in this union; it is not an exact retained-header count.
No mapped system-header instruction or known non-allowlisted standard-library
path was reported. Header macros, constants and other unmapped content still
need review. Earlier captures do not establish complete system-include
coverage, even though their mapped bodies remain available.

This exact candidate passed 35 isolated container checks, the playlist
catalog supplement and 29 HTTPS checks. The fixture mounted the binary and
external standard library on separate operator infrastructure; the owned
containers were removed. Evidence and the joined hashes are under
`.local/native-system-headers-20261004`, with separate container and HTTPS
directories. Its external standard-library hash remains
`5a3f35a0e2e734369effc845ebad8b182912c600ad2114ce400ee3d2502b1ea0`.
There is no installed-player result for this GNU candidate. Production
remains the static image. Non-allowlisted runtime components, include
inputs, unmapped content and distribution review continue to block release;
all records retain `licenseClearance: false`.

## Native preprocessor replay

The preserved `d1275bc2` compiler records were replayed without compiling or
replacing any object. The replay used the exact captured `/usr/bin/cc` hash,
all 1,841 conservative dependency inputs, the original working directories
and compiler flags, and the immutable builder image. Dependency destinations,
object output and compile-only flags were removed for preprocessing. The
container had no network, dropped all capabilities and wrote only to its
owned private output directory and ephemeral source tree.

All 1,034 C and preprocessed-assembly commands completed with both `-E -dD`
and `-E -dU`. The other 39 plain assembly commands have no preprocessor
replay. GCC's [preprocessor documentation](https://gcc.gnu.org/onlinedocs/gcc/Preprocessor-Options.html)
describes `-dD` as emitting definitions with preprocessed output, and `-dU`
as reporting macros expanded or tested for definedness. These observations
therefore include control macros, not just emitted code. Shared mapped
headers also select some compilations whose object code may be unused.

An independent host pass verified all 3,102 compressed records and outputs,
their decompressed hashes and lengths, compiler-trace hashes, parsed
definitions and exact definition-text joins. The two preprocessor modes
produced 1,664,294,821 uncompressed text bytes. The joins report 291,225
system macro use-or-test observations, 948 distinct macro names and 128
system headers. There are no unmatched definition joins; 34,967 observations
have multiple possible system origins, all retained in the records.

A physical-line review of the 248 conservative system inputs found 5,771
macro definitions and four definitions exceeding ten lines. The replay
reported two of those larger candidates: `__SOCKADDR_ALLTYPES` in 1,032
commands and `__tobody` in two. Neither pthread cleanup candidate appeared
in this joined use-or-test set. This is a review queue, not a determination
of retained executable content or applicable license terms. A whitespace-
normalized notice search found LGPL-2.1-or-later text in 159 inputs, SPDX
markers requiring review in 35, GCC exception text in five, and no classified
notice in 49. Those search categories do not assign licenses to the headers
or authorize bundling them under the project policy.

Evidence is under `.local/native-macro-replay-20261004d`, with the exact
compressed outputs in its private WSL directory. Earlier failed mount and
permission attempts remain separately recorded; the three-command pilot
also passed before the complete replay. No server, dependency, build flag,
package allowlist or production image changed. Header contributions,
constants, inlined and unmapped content, external-runtime distribution and
whole-binary licensing remain uncleared.
