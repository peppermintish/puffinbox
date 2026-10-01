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

This inventory is not complete license clearance. Source-path metadata does not establish exact source bytes, and leading comments do not account for included headers, patches, or every retained section. These gaps remain open.

The builder's Debian musl copyright file and Rust's bundled libc are separate evidence. A notice for the builder package does not identify the target archive's version or clear its linked subset.

## Panic-abort probe

A separate build used `CARGO_PROFILE_RELEASE_PANIC=abort` in the same isolated builder. It succeeded with binary SHA-256 `73d023b8be6cf52ae447f658a0cc2f4ff703d2198b706235d79294e098298936`. Its map still selects 389 libc members and five unwind members; its symbols still include core Unicode tables and unwind functions. Changing this profile alone does not remove the identified blockers. No production panic setting was changed. Cargo also [ignores this setting for ordinary tests](https://doc.rust-lang.org/cargo/reference/profiles.html#panic), so passing the standard suite would not validate an aborting production build.

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
