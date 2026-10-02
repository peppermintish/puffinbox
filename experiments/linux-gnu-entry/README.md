# GNU process-entry experiment

These original MIT OR Apache-2.0 sources test an alternative to the system startup objects and the small `atexit` and `pthread_atfork` wrappers selected by a GNU build. They are an experiment for Linux x86-64 with glibc 2.34 or newer. The production Dockerfile still builds the static musl server.

`entry.S` translates the initial process stack into the public `__libc_start_main` call. It preserves the loader's cleanup callback and gives the call a correctly aligned stack. The null initializer relies on the [glibc 2.34 binary contract](https://sourceware.org/pipermail/glibc-cvs/2021q1/072013.html). The [AMD64 process-entry rules](https://refspecs.linuxfoundation.org/LSB_5.0.0/LSB-Core-AMD64/LSB-Core-AMD64/processinitialization.html) and [LSB initialization interface](https://refspecs.linuxfoundation.org/LSB_5.0.0/LSB-Core-generic/LSB-Core-generic/baselib---libc-start-main-.html) describe the calling boundary.

`compat.S` bridges executable calls to the LSB [exit-registration](https://refspecs.linuxfoundation.org/LSB_5.0.0/LSB-Core-generic/LSB-Core-generic/baselib---cxa-atexit.html) and [fork-registration](https://refspecs.linuxfoundation.org/LSB_5.0.0/LSB-Core-generic/LSB-Core-generic/baselib---register-atfork.html) interfaces. Linker wrapping redirects references before archive extraction. Accepting duplicate definitions would leave the libc objects selected and defeat the experiment.

Run the small lifecycle fixtures with:

```sh
python3 scripts/check_gnu_runtime_entry.py
```

The fixtures check constructors, arguments and environment, independent thread-local values, exit status, cleanup order, fork callback order, and Rust panic catching. Their libc and Rust standard library are external test runtimes.

For a server link inventory, use a fresh output directory:

```sh
probe_dir=$(mktemp -d)
cc -c experiments/linux-gnu-entry/entry.S -o "$probe_dir/entry.o"
cc -c experiments/linux-gnu-entry/compat.S -o "$probe_dir/compat.o"
CARGO_BUILD_JOBS=2 RUSTFLAGS='-C prefer-dynamic' cargo rustc --locked --release \
  --target x86_64-unknown-linux-gnu --target-dir "$probe_dir/target" \
  --bin puffinbox-server -- \
  -C link-arg=-nostartfiles \
  -C link-arg=-Wl,--wrap=atexit,--wrap=pthread_atfork \
  -C "link-arg=$probe_dir/entry.o" -C "link-arg=$probe_dir/compat.o" \
  -C "link-arg=-Wl,-Map,$probe_dir/server.map,--cref"
```

Keep the map, symbols, ELF dependencies, compiler identity, and exact runtime hashes with any result. A dynamic link still requires its Rust shared standard library and GNU libraries. This ordinary server probe retains a generated Unicode whitespace lookup and compiler builtins; the [shared-runtime follow-up](../linux-gnu-runtime/README.md) tests moving those inputs outside the executable. Neither experiment clears the MIT/Apache distribution boundary or authorizes release packaging. See [the runtime audit](../../docs/runtime-link-audit.md).
