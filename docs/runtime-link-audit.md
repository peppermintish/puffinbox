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

The linker's archive-inclusion section names 389 selected members of Rust's bundled `self-contained/libc.a` and five members of `self-contained/libunwind.a`. Selection does not prove that every section of a member survived garbage collection. Counts use complete archive/member header lines before `Discarded input sections`; cross references and later section entries are excluded.

The startup inputs include `rcrt1.o`, `crti.o`, `crtbeginS.o`, `crtendS.o`, and `crtn.o` from the target's self-contained directory. Their exact binary provenance and retained sections need review. Rust's [musl toolchain script for 1.98.1](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/src/ci/docker/scripts/musl-toolchain.sh) pins a musl-cross-make revision, GCC 9.2.0, musl 1.2.5, and two musl security patches. GCC's [startup source license header](https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-9.2.0/libgcc/crtstuff.c) specifies GPL with the GCC Runtime Library Exception. A runtime exception is not an MIT or Apache-2.0 license; matching that source to the shipped objects remains part of the audit.

The unstripped server contains `core::unicode::unicode_data` symbols. The compiler's `COPYRIGHT-library.html` assigns Unicode-3.0 to the corresponding data. The current binary therefore cannot be described as meeting the requested boundary.

The selected unwind members include `UnwindLevel1`, register save/restore, and `libunwind` objects. LLVM's [libunwind license](https://raw.githubusercontent.com/llvm/llvm-project/main/libunwind/LICENSE.TXT) includes Apache terms with LLVM exceptions and a legacy license section. The exact compiler-bundled revision and file lineage still need to be established. The legacy section has not been treated as blanket MIT clearance for this archive. Compiler builtins need the same file-level review.

## Musl subset review

The official [musl 1.2.5 archive](https://musl.libc.org/releases/musl-1.2.5.tar.gz) was downloaded for its notices. Its SHA-256 is `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4`. No implementation was copied into Puffinbox.

Leading license comments were checked for the selected math members `ceil`, `ceilf`, `floor`, `floorf`, `pow`, `pow_data`, `rint`, `rintf`, `round`, `trunc`, `truncf`, `exp_data`, and `frexpl`. The power and exponential data files identify MIT; the other checked files have no separate leading copyright notice and fall under the archive's stated default notice. This limited review does not clear all 389 selected libc members or prove their exact relationship to the patched compiler archive.

The builder's Debian musl copyright file and Rust's bundled libc are separate evidence. A notice for the builder package does not identify the target archive's version or clear its linked subset.

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
