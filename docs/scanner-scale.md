# Scanner scale benchmark

Two bounded runs passed on 2026-10-02 against source `656014714988440c4f1fdc4ff7240eb55d51706d` and the tested audio core image. Each run verified exact catalogue counts, deleted its generated shards, and confirmed that stale cleanup retained only the configured library root.

| Generated files | Initial catalogue rows | Initial scan | Stale cleanup | Peak server RSS |
| --- | --- | --- | --- | --- |
| 20,000 | 20,158 | 17.074 s | 1.702 s | 31.66 MiB |
| 99,000 | 99,775 | 39.364 s | 7.311 s | 31.69 MiB |

The larger run indexed 2,534.7 rows per second and removed 99,774 stale rows, leaving one root row. These are single trials on WSL2, kernel `6.18.33.2-microsoft-standard-WSL2`, with 12 logical processors, Docker 29.1.3, and a disposable `postgres:18.6` container. The host also had local acceptance stacks and a runtime audit in progress. Cache and host-load states were not controlled, so the two timings should not be used to infer linear scaling.

For these runs, an isolated helper extracted the existing server from core image `sha256:b023b350118043c20e8c9c428bc0dd2eb856d29f7a6dfb9b328e34bebe1329ad` and substituted that binary for the harness's build step. Its SHA-256 is `191db3e90a653e7fbb392242c7f5df19aef06d720b1b5094e53dfc107aa60e2a`. The helper checked current Rust source hashes against the image manifest. No server source or benchmark logic was changed. Results and ownership records are preserved under `.local/scanner-image-20261002`; each disposable server, database, and generated media tree was removed after its run. The extracted test binary remains in its owned audit directory.

Empty-file indexing does not validate decoding, real storage capacity or latency, concurrent playback, or the requested petabyte, billion-file, and thousand-stream targets. The operational-scale release gate remains open.

`python3 scripts/scanner_scale.py` runs a repeatable, bounded library scan against a disposable synthetic tree and a disposable PostgreSQL container. It builds and launches the current release server directly. It does not read `.local/acceptance/acceptance.env`, load Compose settings, reuse an acceptance container, or accept a database URL or media path from the environment or command line.

The default run creates 2,048 empty `.mkv` files in 16 generated shard directories under `/tmp`. The scanner also indexes the configured media root itself, so the initial catalogue contains 2,065 rows: 2,048 files, 16 shard directories, and one library-root folder. It creates a random, named `postgres:18.6` container, a generated database password, and a randomly assigned port published only to `127.0.0.1`. After PostgreSQL reports readiness inside the container, the harness sends a PostgreSQL v3 startup packet through that loopback-published port and waits for an authentication challenge before it starts the server. It does not pass a host or named volume to Docker; if the base image declares an anonymous data volume, `--rm` removes it with the container. The PuffinBox process receives that database URL, binds its HTTP listener to another loopback port, and uses a fresh temporary data directory. The script bootstraps a temporary administrator and creates one library whose only location is its newly generated temporary media root. On completion or failure, it stops the server, stops only the PostgreSQL container carrying the run's unique safety label, and removes the script-owned temporary directory.

The first pass records server scan counters, exact catalogue row count from the disposable database, wall time from the refresh request through the first completed status poll, throughput, and the server process's Linux `VmHWM` high-water RSS where `/proc` exposes it. The script then removes all generated shard directories while retaining the original media-root directory inode, performs a second scan, and verifies that stale cleanup removes all 2,064 file and shard rows while preserving the one configured library-root row. This second pass exercises the scanner's stale leaf deletion loop, including its 1,000-row committed chunks when the generated catalogue exceeds one chunk.

Example with a larger but still capped tree:

```sh
python3 scripts/scanner_scale.py --files 20000 --files-per-directory 128
```

The default is suitable for a modest local WSL/Linux run. The hard limit is 100,000 total catalogue rows, including generated files, shard directories, and the configured library root; the run is intentionally single-library and single-scan-worker. Empty files measure directory enumeration, metadata lookup, database upsert, and stale-row cleanup. They do not measure media decoding, real storage latency, network filesystems, concurrent users, or arbitrary library layouts. The walker currently uses a 128-slot message channel and 128-item upsert buffer. Each upsert also queries all active recording paths for the library, so memory use for libraries with many active recordings is not represented by this benchmark.

Timing includes HTTP request/response and status polling (up to roughly 200 ms between polls, plus request latency). `VmHWM` is a cumulative process high-water mark for the whole run, not a per-scan allocation measurement; it is reported as `null` where unavailable. Filesystem cache state, CPU, PostgreSQL settings, Docker version, and the host all affect results. Record the printed JSON alongside the machine and run configuration when comparing runs. A small synthetic run is evidence only for that run on that machine; it does not establish petabyte-scale or billion-file support.

The deterministic item-count regression can be run without services:

```sh
python3 scripts/test_scanner_scale.py
```

Requirements: WSL or Linux, Python 3, Rust/Cargo, and a running Docker Engine accessible to the current user. The benchmark script pulls `postgres:18.6` if needed. It does not change any acceptance project or personal media.
