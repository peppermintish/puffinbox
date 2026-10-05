# Acceptance testing

Puffinbox is partial and unreleased. Passing the checks below establishes their stated scope; it does not establish complete Jellyfin 12 compatibility or the requested production scale. See [compatibility](compatibility.md) and [release gates](release-gates.json).

## Current local results — 2026-10-06

The compatible-license correction restores upstream packages, uses rustls, removes project C/assembly and obsolete runtime experiments, and forbids unsafe Rust in every project Cargo target. A narrow SQLx patch preserves PostgreSQL SCRAM behavior for valid Unicode roles and normalized or raw-fallback passwords. The media worker retains filesystem, syscall and resource confinement through safe dependency APIs.

| Check | Result | Scope |
| --- | --- | --- |
| Formatting, strict Clippy and source policy | Passed | All project targets; GNU and musl dependency graphs select rustls without OpenSSL TLS packages. |
| Standard Rust suite | 247 passed, 0 failed, 30 ignored | Ignored cases require a disposable database and are run separately. Includes safe-worker confinement, capability closure and graceful/forced shutdown. |
| PostgreSQL suite | 30 passed, 0 failed | 27 integration cases and three database-backed unit cases, including real SCRAM logins, rejected proofs and Next Up. |
| Python suite | 27 passed | Retired GNU/source-fork tests were removed with their unused implementation. |
| Source browser checks | Nine groups passed | Client identity, native adapter, heartbeat, offline cache, books, and Chromium offline/Live TV/playlists; delayed response bodies included. |
| PDF asset restoration | 12 Chromium cases passed | Current source: Japanese CMaps/text, Foxit Symbol, JPEG2000/JBIG2 pixels, both JavaScript decoder fallbacks, and worker cleanup after rendering or while loading. All 194 retained upstream files and nine notices verified. |
| Packaged PDF reader | Four authenticated browser cases passed | The rebuilt operator image rendered all four original PDF fixtures and released its workers. All 194 packaged assets and nine packaged notices matched their reviewed hashes. |
| Outbound HTTPS | Four cases passed | Correct trust and hostname accepted; wrong issuer or hostname rejected. |
| Dependencies and notices | Passed | Full cargo-deny checks, GNU license audit and complete notice generation under the compatible permissive policy. |
| Images | Passed | Static non-root core, retained notices, operator FFmpeg fd protocol and H.264/AAC encode/probe. |
| Semantic container acceptance | 35 passed, 0 failed, 0 pending | Authentication, policies, media, scanning, subtitles, music, WebSockets, active FFmpeg shutdown and restart/resume. |
| Local HTTPS proxy | 29 passed | Certificate checks, forwarding spoof rejection, secure cookies, media-token scope, logout and active remote-policy changes. Synthetic addresses; no Internet deployment. |
| Public-schema route report | 108 / 364 exact method/path declarations | Declaration coverage only. |

The original corrected core is `sha256:6d66ba262dfac268acfa49ec6d6894d2c8b2a8edcd582735410cb1f25e80a841`; its operator image is `sha256:53eacf6d1359d2eb1cdce8a8def952768861b9d41cbafbd9ea377fddbb07cae0`. Both were built from the corrected working tree on checkpoint `ece05bd`, with exact source fingerprints in the local ledger. They are not builds of the preceding commit alone.

The subsequent PDF restoration was built on checkpoint `d7417dd`: core `sha256:ac1ae481c3034b3ef2f0dd537f79f5cd0ba416de4c262150df7f5699eb3a5fe7`, operator `sha256:d5f9882fd84675ed1775f04d6491a9fbb750c7e0e9552caa723163043901e8c2`. A fresh isolated tree passed all 35 container checks, followed by the four authenticated PDF browser cases. Source fingerprints, asset hashes, notices, screenshots and cleanup records are in `.local/pdf-assets-20261005/`. Its synthetic browser cookie was revoked and its stack stopped with state preserved. Japanese glyphs displayed correctly on the tested Windows browser; unembedded font appearance on other hosts remains dependent on installed fonts. Documentation changes after these tests do not alter the tested implementation.

The first container attempt passed twelve checks, then failed because its copied fixture tree lacked the newly required embedded-audio files. The second fresh tree ran the current supplemental generator and passed all 35 checks. Both records are preserved; no acceptance assertion was weakened.

Next Up has source and disposable-database coverage for the [observed episode-history contract](jellyfin12-next-up.md), including skipped gaps, resumable filtering, cutoff, completed shows, rewatching, paging and selected-user policies. The full source and 30-case database run passed before the final dual-candidate rewatch correction; that correction then passed strict Clippy and its targeted database regression. A local compiler memory failure prevented the first targeted retry from starting tests; the fresh retry passed.

The first Next Up core (`sha256:c070b09311922b70d47edf13b373a646e1143a66b44f0bb608535fc8fe42c6c8`) and operator (`sha256:9b563540bde73ae3fd4fdf006e6fc69b87d6956f78b7cf21bd28f9ed1afdc89c`) were built from the working tree at `601c817`, with exact fingerprints under `.local/next-up-contract-20261005/images/`. The isolated operator passed all 35 container checks and 52 selected public-reference projections. Those comparisons required Puffinbox's explicit local NFO refresh after scanning; the first setup attempt had not imported the display titles. Fixture hashes stayed unchanged. Wider DTO fields, server-specific IDs and season folder labels were excluded from the comparison as documented in its ledger.

Official web exposed repeated-array and date-only cutoff failures on subsequent images. The parser now accepts repeated Fields and image types and exact calendar dates at UTC midnight, with scalar-duplicate, encoded-delimiter and invalid-date regressions. At `1f971e4`, formatting, strict Clippy, source policy, 247 standard Rust cases, the targeted Next Up database regression and the offline, Live TV, playlist and twelve PDF browser checks passed. A delayed dialog-close event had cancelled verification after an offline dialog was reopened; the corrected browser regression reproduces that sequence and checks old audio and blob cleanup.

The date-compatible core is `sha256:e908f7f73aae1cc8f4a2053df29d85c633ebd3284e26f4f848944f4aa37bedae`, with operator `sha256:54c7b17603a2fcdd8e436249098bc05d86805c40fdeeb29a367fc92a3d0da7f6`. It passed 35 fresh container checks and 57 preserved Next Up projections, including five date and complete-client-query observations. Both official clients displayed the expected Next Up card. Its watched and favorite writes returned 404 because the clients used per-user action paths. A visually changed watched button did not establish a stored write.

At `53ac450`, those action aliases passed strict Clippy and the user-data database regression. Core `sha256:a534975f97e6b7cc65e5f7fa5baa9f29c0a88bee23be997b0d007ff2a619b00b` and operator `sha256:cf88d1dacbb652e7b42d5747f4206b9673497f34d66d42f4e8ad2d3dc1174658` passed packaging and runtime inspection. The existing client stack was upgraded while preserving server identity, episode user data and fixture hashes. This image has the client action checks below; the preceding image's local 35/57 runs are not fresh runs on this upgrade. Exact fingerprints, comparisons, screenshots and per-action reads are under `.local/next-up-contract-20261005/`.

The restart check stopped the server while an FFmpeg HLS process was active. It verified process shutdown, playback-row closure, the committed resume position, catalog and embedded metadata persistence, stale HLS-session rejection, and reconnected socket notifications. Independent HLS probes cover the 300.023-second source, nonsequential segment timestamps and the final boundary. Audible continuity across batches remains unvalidated.

## Official clients

Unchanged official web assets and the verified Qt 6 Jellyfin Desktop executable were tested against the original corrected operator image through a loopback observation proxy. API responses were forwarded unchanged. A fresh synthetic account and Desktop profile were used; Remember Me was off. Older libraries, grants, media and saved playback rows were preserved.

Official web visibly decoded the long synthetic HLS fixture at 640×360. Its timeline advanced from 18.830183 to 39.670778 seconds; normal navigation stopped playback and committed 51.948079 seconds. Both clients completed the four-track tagged FLAC album automatically. Each queue generated fifteen successful playback reports and added exactly one play per track; music resume positions stayed zero. A web replay did the same, and active decoded browser audio was observed. All thirteen fixture hashes remained unchanged.

Desktop accepted original MP4 direct-play requests, progress and stops, but the earlier profile's captured video surface was black. A subsequent fresh default-renderer profile on operator `9b563540` displayed the long HLS fixture advancing from its burned-in 12.250-second frame to 46.792 seconds. Twenty-eight progress reports succeeded; pausing committed 75.5 seconds. Closing that owned window produced no stopped report, so this does not establish normal Desktop stop behavior or general video acceptance. Audible output/continuity, broader formats and clients remain open. A transient web playback notice was observed before successful HLS playback; its cause is unresolved. Synchronized playback remains incomplete. These results do not clear the behavioral compatibility gate.

On operator `cf88d1da`, unchanged official web and the verified Desktop executable each completed watched, favorite and both undo actions on the same synthetic episode. All eight per-user requests returned 200. Independent reads after each action confirmed the flags, play count and Next Up selection; fresh client views excluded the watched episode and restored it after undo. Original episode user data and fixture hashes were restored, with all other episode state unchanged. Remember Me stayed off. Desktop's Favorites page separately returned 400 for `SeriesSortName`; the [source sorting correction](jellyfin12-series-sort.md) has a database regression, while its packaged Favorites retest remains pending. The generic Next Up list displays `0-0 of 0` when its request disables totals, despite showing the selected cards. This display limit remains qualified.

Private evidence is under `.local/policy-correction-20261005/`: `source2`, `browser`, `images`, `image-inspection`, `container2`, `remote`, `clients` and `native`. `clients/verified.json` joins the tested images to 113 implementation fingerprints, queue reports, independent user-data reads and screenshot hashes. Credentials and full client logs remain ignored by Git.

## Cloud and earlier records

The Rust and compatible-license correction at `d7417dd` passed [both cloud jobs](https://github.com/peppermintish/puffinbox/actions/runs/37229872635), including the isolated media/restart acceptance and static Linux build. The PDF restoration's [diagnostic run at `8e70984`](https://github.com/peppermintish/puffinbox/actions/runs/37233830813) passed isolated media acceptance but failed the source browser check: the unembedded Japanese fixture rendered blank. The source and release browser runners install `fonts-noto-cjk`; text and pixel assertions remain unchanged. All twelve cases passed locally in Linux Chromium and [both cloud jobs passed at `601c817`](https://github.com/peppermintish/puffinbox/actions/runs/37235082365). Those fonts are external browser test prerequisites, not assets bundled with Puffinbox.

The [Next Up run at `7e80520`](https://github.com/peppermintish/puffinbox/actions/runs/37360527741) passed isolated media acceptance but failed the source browser job. The offline reopen race was reproduced locally and fixed; [both jobs passed at `1f971e4`](https://github.com/peppermintish/puffinbox/actions/runs/37363359552), including the browser checks and static build. [Both jobs also passed at `53ac450`](https://github.com/peppermintish/puffinbox/actions/runs/37366181589), which adds the per-user watched and favorite aliases. Each outcome applies to its named source.

The preceding `ece05bd` passed [both main jobs](https://github.com/peppermintish/puffinbox/actions/runs/37199589069) and [both retired runtime jobs](https://github.com/peppermintish/puffinbox/actions/runs/37199589042). Each result applies to its named source.

The full earlier acceptance narrative is retained [at checkpoint ece05bd](https://github.com/peppermintish/puffinbox/blob/ece05bdfca3566e4d6d241653d238446d5fc76c8/docs/acceptance.md). The shorter [history](validation-history.md), feature-specific contract documents and private ledgers retain their original artifact scope. The exact-license GNU/OpenSSL experiments are retired.

## Requirements

- WSL Ubuntu or another Linux environment with Python 3, FFmpeg, and ffprobe.
- For the container workflow below, Docker Engine, the Docker Compose plugin, and a Linux account that can access the Docker socket. Do not loosen socket permissions to make the harness run.
- For direct-source WSL checks, Rust/Cargo and a dedicated disposable PostgreSQL database reachable from WSL. Do not use a production database.
- For container checks, the repository and fixture directory must be available to the Docker daemon.

The service listens on `http://127.0.0.1:18096` and PostgreSQL is published only on `127.0.0.1:55432`. The generated operator-tool image installs FFmpeg from Ubuntu packages. That combined local image is only a test runtime; its FFmpeg build, linked libraries, codec duties, license terms, and any source obligations remain separate from the Puffinbox server image.

For wildcard binds behind a reverse proxy, set `PUFFINBOX_PUBLIC_BASE_URL` to the public HTTP(S) origin (for example `https://media.example.net`, without a path, credentials, query, or fragment). An HTTPS origin requires `PUFFINBOX_TRUSTED_PROXIES` to contain the proxy's actual CIDR and `PUFFINBOX_COOKIE_SECURE=true`. The trusted proxy CIDR lets the server evaluate remote-access policy against the client address forwarded by that proxy. `System/Info.LocalAddress` stays null when a wildcard bind has no configured public origin.

PostgreSQL integration targets annotated with `#[ignore]` are intentional: they create isolated schemas or exercise destructive state and require an explicitly disposable database. The source and release workflows configure an explicit list of these targets for a temporary PostgreSQL service, with cloud results tracked separately from local results. Add each new ignored PostgreSQL target to both workflow lists when it is introduced; `cargo test --all-targets` alone does not execute ignored tests.

To repeat the HTTPS proxy check on Linux, use an already-built core image and a fresh output directory:

```sh
docker pull postgres:18.6-alpine
docker pull python@sha256:7c61056e61ac89e852de05f3dc6fa51a6dd2181797bceed46aa725dd7cb2cd3b
python3 scripts/check_remote_access.py --image puffinbox:local \
  --tools-image python@sha256:7c61056e61ac89e852de05f3dc6fa51a6dd2181797bceed46aa725dd7cb2cd3b \
  --output-dir .local/remote-access-new-run
```

The host needs Docker, Python 3.13 or later, and OpenSSL. The proxy runs as the Linux host account to read its private fixture files without broadening permissions. The proxy script is a bounded test fixture, including a synthetic-address selector; it must not be deployed as a public reverse proxy. Its runtime and PostgreSQL are external test inputs and are not added to the project release image. Both workflows run this check and retain the sanitized result ledger.

The Live TV source/guide/policy/timer regression is available locally as `tests/postgres_livetv.rs`. Run it only against a disposable PostgreSQL database by setting `PUFFINBOX_TEST_DATABASE_URL` in the environment; the test creates and drops its own random schema and does not print the connection string:

```sh
cargo test --locked --test postgres_livetv -- --ignored --nocapture
```

The target is included in both workflow lists.

The limited PDF/EPUB reader and its library, parental-rating, and download-policy checks are in `tests/postgres_book_reader.rs`. Run it only against a disposable PostgreSQL database:

```sh
cargo test --locked --test postgres_book_reader -- --ignored --nocapture
```

The source and release workflows include this target.

The cookie-backed native media authorization regression is in `tests/postgres_media_access_tokens.rs`. It checks that the existing HttpOnly session cookie can still read `/Items/{id}/File`, the cookie-only same-origin exchange issues a bounded token, and that token is limited to its parent session and read-only media routes. Run it only against a disposable PostgreSQL database:

```sh
cargo test --locked --test postgres_media_access_tokens -- --ignored --nocapture
```

The source and release workflows include this target. The target passed 1/1 locally against the isolated disposable PostgreSQL service.

## Prepare and run

From the repository root in Linux, generate the ignored credentials and fixtures:

```sh
python3 scripts/prepare_acceptance.py
python3 scripts/prepare_supplemental_fixtures.py
```

With the default state root, generated settings are saved in `.local/acceptance/acceptance.env`; an alternate state root stores them as `<state-root>/acceptance.env`. The file contains database and test-account secrets. It is ignored by Git; do not print, copy, or commit it. The script refuses to overwrite it or any expected fixture file already present in the selected fixture tree.

For direct-source WSL runs, choose a new, empty fixture directory before the initial preparation, for example `PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT=/tmp/puffinbox-acceptance/media python3 scripts/prepare_acceptance.py`. The generator stores this choice in the ignored settings file and writes synthetic media there. The default invocation above keeps fixtures under `.local/acceptance/media` for the container workflow. The generator refuses expected target collisions; it uses no-overwrite FFmpeg output and exclusive creation for fixture copies.

For a separate acceptance state that leaves `.local/acceptance` untouched, select fresh sibling state and media directories and set `PUFFINBOX_ACCEPTANCE_STATE_ROOT` and `PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT` when preparing. For example, use `/tmp/puffinbox-direct-play-run/state` and `/tmp/puffinbox-direct-play-run/media`. The generator assigns a distinct PostgreSQL database/user, Compose project, web port, and PostgreSQL port for this alternate state. `MEDIA_ROOT` and the fixture-root setting point to the host media directory; the library roots in the generated settings use the Compose `/media` mount. For a direct-source server, pass `--host-media-paths` to the harness so it configures the library with host paths. Run the harness with both `--env-file <state>/acceptance.env` and `--results-file <state>/acceptance-results.json`; the alternate result file must be new and inside the state directory. `--direct-play-only --skip-container-restart` still runs readiness and health checks, bootstrap and authentication/session checks, origin and token handling checks, and isolated library setup; its media checks are limited to the MP4 scan, PlaybackInfo negotiation, and authenticated full/range assertions. Start it only against a separately disposable database and source server configured from that alternate file. The harness refuses alternate paths that overlap the active acceptance tree or reuse its database/user, project, fixture tree, or loopback server port.

The generated video fixtures include a 12-second incompatible MPEG-4/MP3 Matroska transcode/subtitle fixture, a 300-second incompatible MPEG-4/MP3 HLS shutdown fixture, and a small H.264/AAC MP4, and a 12-second H.264/AAC MP4 with explicit SDR signalling. The HTTP harness negotiates the MP4 with an explicit matching Jellyfin-shaped `DeviceProfile`, then checks authenticated full and ranged `/Videos/{id}/stream` responses for original bytes and source MIME, length, range, and safety headers. The current container run passed the MP4 byte checks and the separate active-FFmpeg restart check. These bounded checks do not establish broader client compatibility. `ffmpeg` must provide `libx264` plus AAC encoding.

Build the project-only image, then the separate local FFmpeg tools image:

```sh
docker build --tag puffinbox:local --file Dockerfile .
docker build --tag puffinbox:acceptance-tools --file Dockerfile.acceptance-ffmpeg .
```

Use this same Compose file set for build, startup, restart checks, and cleanup:

```sh
docker compose --project-name puffinbox-acceptance --env-file .local/acceptance/acceptance.env \
  -f docker-compose.yml \
  -f docker-compose.acceptance.yml \
  -f docker-compose.ffmpeg-runtime.yml build server

python3 scripts/check_operator_runtime.py --image puffinbox:operator-ffmpeg

docker compose --project-name puffinbox-acceptance --env-file .local/acceptance/acceptance.env \
  -f docker-compose.yml \
  -f docker-compose.acceptance.yml \
  -f docker-compose.ffmpeg-runtime.yml up -d --wait --wait-timeout 120

python3 scripts/acceptance.py --require-scan --require-transcode
```

The harness keeps its result ledger at `.local/acceptance/acceptance-results.json`. It checks database readiness for more than 30 seconds, bootstrap and header authentication, token revocation, cross-origin and conflicting credentials, user/library policy enforcement, scan create/modify/delete reconciliation, graceful service restart and catalog persistence, exact byte-range hashes, browser-safe photo delivery, playback-session ownership/progress/stop/resume data, positive H.264/AAC MP4 direct-play negotiation and authenticated original-byte full/range reads, and an incompatible Matroska file converted to H.264/AAC HLS with embedded subtitle extraction and cancellation.

The harness deliberately fails when a required semantic check fails. A failure report is local evidence, not a reason to soften an assertion without checking the server behavior. Test accounts and paths have the `acceptance-` or `Puffinbox Synthetic` prefix.

Open `http://127.0.0.1:18096/web/` for separate manual browser or installed-client review. The test credentials are in the ignored settings file; they are not echoed by the harness. Loading this original page inside Jellyfin Media Player is distinct from native mpv integration or broad Jellyfin API compatibility. Record those client outcomes separately.

After review, stop the test stack and remove only its isolated volumes:

```sh
docker compose --project-name puffinbox-acceptance --env-file .local/acceptance/acceptance.env \
  -f docker-compose.yml \
  -f docker-compose.acceptance.yml \
  -f docker-compose.ffmpeg-runtime.yml down --volumes --remove-orphans
```

The GitHub Actions acceptance job is configured to run this flow with synthetic data. Cloud outcomes are recorded separately; see the [workflow runs](https://github.com/peppermintish/puffinbox/actions).

The assembled-operator FFmpeg check verifies the `fd` protocol used by the server's confined media reader. The host fixture generator does not require that protocol. A successful version print or encode probe alone does not establish that protocol is available.

## Direct-source WSL checks without Docker

The HTTP checks can also run against a server started from this checkout. This exercises the current source process, not a built container image, container filesystem/capability settings, or a container restart. Start a dedicated disposable PostgreSQL instance first and create the database and user named in the generated settings. Then, from the repository root in WSL, load those settings without printing them, set host paths for the synthetic libraries, and start the server:

```sh
while IFS='=' read -r key value; do
  case "$key" in ''|\#*) continue ;; esac
  export "$key=$value"
done < .local/acceptance/acceptance.env

export DATABASE_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@127.0.0.1:5432/${POSTGRES_DB}"
export PUFFINBOX_BIND=127.0.0.1:18096
export PUFFINBOX_DATA_DIR="$PWD/.local/acceptance/wsl-data"
export PUFFINBOX_WEB_ROOT="$PWD/web"
export PUFFINBOX_FFMPEG_PATH="$(command -v ffmpeg)"

cargo run --locked --bin puffinbox-server
```

In a second WSL terminal, run the HTTP harness with its single container-only check explicitly skipped:

```sh
python3 scripts/acceptance.py --require-scan --require-transcode --skip-container-restart --host-media-paths
```

The result ledger records `Container/runtime restart validation` as `pending`; the option skips only that final check, and the preceding HTTP assertions still run. To complete container/runtime restart validation, use the standard Compose workflow above and rerun the harness without `--skip-container-restart`. A direct-source run must never be reported as container validation.
