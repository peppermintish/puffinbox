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

The public recipe also passed from a fresh disk-backed output directory on 2026-10-02, producing server SHA-256 `be3f42165eb98c9b520c17b9d7e5305b3fe52c21be91c71a98c89be2c5cea165` and the same external standard-library hash. Its map inventory and 4,096-row numeric differential passed, with all 11 baseline definitions and alternative imports verified and no Cargo jobserver descriptor warnings. This reproduction has not yet repeated the server's 25 container or 29 HTTPS assertions; those results above belong to the exact `38516f9e` binary. Records are under `.local/runtime-reproduction-disk-20261002`. Earlier recipe failures remain separate: an attribute-count guard, the direct fixture's duplicate core dependency, and exhausted RAM-backed `/tmp`. Generated compilation caches were preserved on disk, with relocation records; fixtures and test ledgers were retained.

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
