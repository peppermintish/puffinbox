# Acceptance testing

Puffinbox is partial and unreleased. The source and container checks below passed locally on 2026-10-02. Earlier browser checks remain applicable to unchanged interface code; client observations identify their tested image and scope.

| Check | Result |
| --- | --- |
| Rust formatting and strict workspace Clippy | Passed |
| Standard Rust workspace suite | 220 passed, 0 failed; 23 database cases skipped by default |
| Disposable PostgreSQL regression suite | 22 integration cases and 1 database-backed unit test passed |
| Python acceptance and route-report tests | 23 passed |
| Browser helpers, playlists, Live TV, offline cache, and book reader | Passed; book reader has 8 Node cases |
| Strict Cargo dependency audit and full notices | Passed with MIT and Apache-2.0 as the only allowed licenses |
| Static Linux server and scratch image build | Passed |
| Outbound HTTPS verification | Trusted CA accepted; wrong CA and wrong host rejected in both the source process and scratch container |
| HTTPS proxy and remote-access policy | 29 local checks passed in an isolated Docker fixture |
| Isolated container acceptance | 24 passed, 0 failed |

The container result applies to operator test image `sha256:90ca0933f76ab4fa01acbe6701d8bdfd3a603120a838aa8ed8d29ea1c5386152`, assembled from core image `sha256:f4d534f217a3dddb5ea7eaffad0fc35ad9b08a3ee7e637f02ea4afd314919d42`. It was built from the working tree at `f33c138`, including catalog filters and their cache-header fix. The run finished at 20:24 UTC on 2026-10-01 (2026-10-02 locally). The server used its bundled web files; no source overlay was mounted. Its FFmpeg tools are supplied separately from the server image. The generated ledger, `acceptance-filters-cache-20261002-results.json`, is kept in ignored local storage, alongside synthetic fixtures and credentials. The ledger records a dirty working tree and no commit build label. Earlier HLS and socket ledgers remain preserved. The first filter run failed on a missing cache header; its result is preserved separately, and the rebuilt image passed the unchanged assertion.

The same core image passed 29 HTTPS proxy checks using a separate database, accounts, private backend network, and loopback TLS listener. The external Python test image was pinned by digest. Certificate verification accepted the fixture CA and rejected a different CA and hostname. The checks covered the advertised HTTPS origin, secure cookies, local and remote login, active-session policy changes, spoofed forwarding headers, untrusted peers, same-origin token exchange, CORS and fetch-metadata restrictions, and logout. The passing ledger is preserved at `.local/remote-access-20261002-f/results.json`; the fixture removed only resources carrying its own ownership label. Earlier fixture setup failures are preserved separately. This is a local proxy test with synthetic remote addresses, not an Internet-facing deployment or comprehensive security review.

Catalog filters have a real PostgreSQL regression for NFO tags and years, preferred provider fields, stable genre IDs, category combinations, pagination/counts, private and disabled libraries, hidden paths, parental policy, changed permissions, plugin disablement, invalid input, and the 4,096-choice bound. The container check verifies both public DTO shapes, private cache headers, a known genre selection, unknown IDs, and invalid years. A synthetic movie sidecar supplied `Documentary`, `Clock test`, and `2020`. Official Jellyfin web displayed these choices; selecting all three narrowed three movies to that one item. After container restart and a fresh login, the selected filters still returned the same item. The screenshot and unchanged-response proxy trace are preserved locally. Stream-language choices and Live TV classification selectors remain unsupported.

The restart check starts a 300-second HLS fixture, serves a playlist and segment, commits its playback position, and requests a fresh transcode batch before stopping the container while FFmpeg is active. It requires an exit code of zero, no forced decoder termination, and confirmation that all media children drained. After restart it checks catalog persistence, playback-row closure, the committed resume position, and rejection of the old HLS session. This run passed all those assertions. Unit tests also cover a cooperating child and a child that ignores termination, including reaping and the bounded forced-stop fallback.

Authenticated sockets passed keepalive exchange and committed `UserDataChanged` delivery, with another account receiving no notification. The restart check received close code 1001 before the process exited, reconnected afterward, and received fresh notifications. The PostgreSQL socket target uses real TCP upgrades and checks scoped-token rejection, conflicting credentials, browser origins, connection admission, oversized or malformed messages, message rate, expiry, disabled accounts, remote-access changes, revocation, changed item visibility, stale-run fencing, and shutdown. A local Clippy attempt hit memory pressure during concurrent builds; the preserved failure was followed by a passing serial run.

The HLS timeline check negotiates a resume at 123.185588 seconds and requires a full VOD playlist without an offset start argument. Its 76 segments cover the 300.023-second source. Nonsequential requests for segments at 120, 8, and 300 seconds produced H.264 video with matching source timestamps and advertised durations. The first segment beyond the source returns 404; cancellation returns 204. This checks the video timeline, not audio continuity across batches or broad codec support.

The current web client completed the synthetic H.264/AAC MP4 and reported that the final position was saved. Official Jellyfin web authenticated, registered session capabilities, listed the synthetic movies, and opened their details through a localhost proxy. Ancestor and theme-media responses have database coverage for library, path, and rating restrictions, inheritance, and cycles. HLS playback required accepting the client's empty subtitle selection and supplying its transcoding protocol and container. With full-duration HLS, Edge decoded the test pattern, sought backward to zero and forward to three minutes, and persisted 188.950346 seconds on stop. A fresh library navigation then resumed from the desktop player's saved 122-second position and stopped at 160.212549 seconds. Its displayed position matched the burned-in source clock.

With socket notifications enabled, the official web client kept a details page open at a seeded 48-second position. An update from a separate authenticated session changed the position to 160 seconds. Without navigation or reload, Resume negotiated 160 seconds and decoded the movie. Stopping saved 196.5659039 seconds; Resume from the same cached details negotiated that exact position and decoded again. The proxy recorded `UserDataChanged` messages for both updates. A brief “Unable to play media” notice appeared during the second start, followed by successful playback; client logs include nonfatal HLS errors. The saved-position behavior passed, but the transient notice and broader playback reliability still need diagnosis.

Socket support covers user-data notifications and keepalive. It has bounds of 512 total connections, eight per user, 16 KiB per message, 32 incoming messages per second, and a 256-event queue. Session authorization is rechecked every five seconds and before private data is sent. Other Jellyfin socket subscriptions and remote-control messages remain unsupported. These bounds have admission tests, not production load validation.

Resume lists now filter by `MediaTypes` before pagination and counting, including intersections with `IncludeItemTypes`. Database checks cover video, audio, photos, books, unknown types, and visibility restrictions. In official Jellyfin web, the movie appears in Continue Watching without appearing in Continue Listening or Continue Reading.

The installed Jellyfin Media Player authenticated, browsed the library, decoded the HLS test pattern in its native player, and paused. It loaded Puffinbox's original web interface, so this is a native-player integration result rather than a test of the desktop client's usual Jellyfin interface. Native resume waits for a decoded origin before seeking to the saved source time; the native load start option had landed at the next segment boundary. An in-place backward seek failed in the installed player, so the original interface now reopens HLS at the requested source position while preserving pause state and track selections. Review passed a resume at 160 seconds, a paused seek back to zero, forward to 99 seconds, and backward to 48 seconds. The visible source clock matched, and stopping persisted 48 seconds with `Played=false`. Broader codecs, audio continuity, and track changes remain unvalidated.

After container restart, the installed desktop player offered the saved 48-second position and resumed playback from it using the rebuilt image's bundled interface.

Those native-player observations apply to the earlier full-duration HLS image. The 2026-10-02 desktop attempt exposed accessibility controls but captured a blank player window. Trying the documented display preference and a localhost official-web connection did not produce a usable result; the original configuration was restored. There is no new native-player pass for the filter image or the desktop client's usual Jellyfin interface.

A separate timestamp probe against the socket image checked three 64-second HLS batch boundaries. Adjacent video packets had no timestamp gap. AAC packets overlapped by about 21–24 milliseconds at each checked boundary. This records packet timing only; audible continuity and its relationship to the client's nonfatal HLS warnings remain unresolved.

The browser regression suite simulates the documented native bridge to check deferred resume, backward stream replacement, pause preservation, position-before-duration event ordering, and progress saved after an early native finish. These checks do not simulate decoding or establish mpv compatibility. An early finish below the end of the source no longer marks the item complete or replaces the last observed position with a post-finish zero.

Full-duration transcoding applies to trusted source durations up to four hours. It generates 64-second batches on demand, using the existing two-encoder limit and 32-session bound. Each session has a 2 GiB output limit and a 60-second idle timeout. There is no cache eviction yet, so a long or high-bitrate movie can exhaust that limit. Unknown-duration and stream-copy paths retain their existing bounded HLS behavior. These limits do not support the requested production stream count.

The demo server reports version 12.1.0, while the route comparison stays pinned to 12.0.0. The proxy forwards API response bodies unchanged and fetches only compiled web assets from the official demo. Public demo PlaybackInfo responses were also compared without reading the server or client implementation. Earlier client observations are [historical](validation-history.md).

The source is published at [peppermintish/puffinbox](https://github.com/peppermintish/puffinbox). [Both cloud CI jobs passed at `b179b9d`](https://github.com/peppermintish/puffinbox/actions/runs/36922094308), covering source, browser, dependency notices, static build, TLS verification, and isolated container acceptance. At [`f33c138`](https://github.com/peppermintish/puffinbox/actions/runs/36909587357), isolated container acceptance passed, but the source job failed in the Live TV browser check while accessing a missing channel control. The test now waits for the retry and refreshed controls to finish; ordinary and throttled local runs and the current cloud run passed. The older test also passed when throttled locally, so the earlier failure has not been conclusively reproduced. Release packaging is blocked by [the release gates](release-gates.json): the Cargo audit does not cover the complete linked runtime, and full API behavior, features, external clients, remote access, and scale still need validation.

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

The host needs Docker, Python 3.13 or later, and OpenSSL. The proxy script is a bounded test fixture, including a synthetic-address selector; it must not be deployed as a public reverse proxy. Its runtime and PostgreSQL are external test inputs and are not added to the project release image. Both CI workflows run this check and retain only the sanitized result ledger as an artifact. A complete cloud result for this newly added check is still pending.

The Live TV source/guide/policy/timer regression is available locally as `tests/postgres_livetv.rs`. Run it only against a disposable PostgreSQL database by setting `PUFFINBOX_TEST_DATABASE_URL` in the environment; the test creates and drops its own random schema and does not print the connection string:

```sh
cargo test --locked --test postgres_livetv -- --ignored --nocapture
```

The target is included in the repository's CI and release workflow lists. See the [GitHub workflow runs](https://github.com/peppermintish/puffinbox/actions) for cloud results.

The limited PDF/EPUB reader and its library, parental-rating, and download-policy checks are in `tests/postgres_book_reader.rs`. Run it only against a disposable PostgreSQL database:

```sh
cargo test --locked --test postgres_book_reader -- --ignored --nocapture
```

This target passed 1/1 locally and in the successful source CI run. The release workflow also includes it; release packaging remains gated.

The cookie-backed native media authorization regression is in `tests/postgres_media_access_tokens.rs`. It checks that the existing HttpOnly session cookie can still read `/Items/{id}/File`, the cookie-only same-origin exchange issues a bounded token, and that token is limited to its parent session and read-only media routes. Run it only against a disposable PostgreSQL database:

```sh
cargo test --locked --test postgres_media_access_tokens -- --ignored --nocapture
```

The source and release workflows include this target. The target passed 1/1 locally against the isolated disposable PostgreSQL service.

## Prepare and run

From the repository root in Linux, generate the ignored credentials and fixtures:

```sh
python3 scripts/prepare_acceptance.py
```

With the default state root, generated settings are saved in `.local/acceptance/acceptance.env`; an alternate state root stores them as `<state-root>/acceptance.env`. The file contains database and test-account secrets. It is ignored by Git; do not print, copy, or commit it. The script refuses to overwrite it or any expected fixture file already present in the selected fixture tree.

For direct-source WSL runs, choose a new, empty fixture directory before the initial preparation, for example `PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT=/tmp/puffinbox-acceptance/media python3 scripts/prepare_acceptance.py`. The generator stores this choice in the ignored settings file and writes synthetic media there. The default invocation above keeps fixtures under `.local/acceptance/media` for the container workflow. The generator refuses expected target collisions; it uses no-overwrite FFmpeg output and exclusive creation for fixture copies.

For a separate acceptance state that leaves `.local/acceptance` untouched, select fresh sibling state and media directories and set `PUFFINBOX_ACCEPTANCE_STATE_ROOT` and `PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT` when preparing. For example, use `/tmp/puffinbox-direct-play-run/state` and `/tmp/puffinbox-direct-play-run/media`. The generator assigns a distinct PostgreSQL database/user, Compose project, web port, and PostgreSQL port for this alternate state. `MEDIA_ROOT` and the fixture-root setting point to the host media directory; the library roots in the generated settings use the Compose `/media` mount. For a direct-source server, pass `--host-media-paths` to the harness so it configures the library with host paths. Run the harness with both `--env-file <state>/acceptance.env` and `--results-file <state>/acceptance-results.json`; the alternate result file must be new and inside the state directory. `--direct-play-only --skip-container-restart` still runs readiness and health checks, bootstrap and authentication/session checks, origin and token handling checks, and isolated library setup; its media checks are limited to the MP4 scan, PlaybackInfo negotiation, and authenticated full/range assertions. Start it only against a separately disposable database and source server configured from that alternate file. The harness refuses alternate paths that overlap the active acceptance tree or reuse its database/user, project, fixture tree, or loopback server port.

The generated video fixtures include a 12-second incompatible MPEG-4/MP3 Matroska transcode/subtitle fixture, a 300-second incompatible MPEG-4/MP3 HLS shutdown fixture, and a small H.264/AAC MP4. The HTTP harness negotiates the MP4 with an explicit matching Jellyfin-shaped `DeviceProfile`, then checks authenticated full and ranged `/Videos/{id}/stream` responses for original bytes and source MIME, length, range, and safety headers. The current container run passed the MP4 byte checks and the separate active-FFmpeg restart check. These bounded checks do not establish broader client compatibility. `ffmpeg` must provide `libx264` plus AAC encoding.

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
