# Puffinbox

Puffinbox is an original, self-hosted media server project. It serves its own web interface and implements a growing Jellyfin-shaped HTTP API for use by compatible clients. Jellyfin API version reporting is a compatibility target; it does not mean every Jellyfin operation, client behavior, or native-player integration is supported.

Puffinbox is unreleased. See [the compatibility matrix](docs/compatibility.md) for supported features and remaining gaps, and [acceptance testing](docs/acceptance.md) for reproducible checks. Loading a page in Jellyfin Media Player alone does not establish playback or API compatibility.

## Run with Compose

1. Copy `.env.example` to `.env` and replace `POSTGRES_PASSWORD` with a long URL-safe random value. Do not commit `.env`.
2. Create the media directory named by `MEDIA_ROOT` and place test or personal media there. The server receives this mount read-only.
3. Run `docker compose up --build -d`.
4. Open [http://127.0.0.1:8096/web/](http://127.0.0.1:8096/web/). On an empty database, the server's startup flow asks for the one-time setup token in its data directory. Alternatively, set both bootstrap administrator variables before the first start.

The standalone server listener defaults to `127.0.0.1:8096`. Compose explicitly binds the server inside its container to `0.0.0.0:8096` and publishes the host port on loopback by default. Do not expose an unencrypted listener to an untrusted network. For a reverse proxy, configure TLS at the proxy, set `PUFFINBOX_COOKIE_SECURE=true`, set `PUFFINBOX_TRUSTED_PROXIES` to the proxy's actual CIDRs, and set `PUFFINBOX_PUBLIC_BASE_URL` to the public origin. HTTPS public origins require at least one trusted proxy CIDR so the server can determine whether each request came from a local client. Set `PUFFINBOX_CORS_ORIGINS` only if a separate browser origin is required. Newly created users have remote access disabled unless an administrator explicitly enables it; this account policy is separate from network reachability.

For an isolated local acceptance stack, use the generated ignored configuration described in [Acceptance setup](docs/acceptance.md). It creates synthetic media under `.local/` and uses separate database and web ports; it does not point at a personal media library.

Outbound HTTPS uses rustls with certificate verification. The project image contains no CA bundle. Supply trust roots with `TLS_CA_BUNDLE` and add `-f docker-compose.tls.yml` to Compose. For a source process, set `SSL_CERT_FILE` to the operator's PEM bundle. Unicode hostnames use standard IDNA encoding.

For a bounded scanner throughput and stale-row cleanup measurement, see [the scanner scale benchmark](docs/scanner-scale.md). It creates a fresh temporary media tree and a separately labeled disposable PostgreSQL container; it does not use the acceptance stack.

## External FFmpeg

The default server image does not include FFmpeg or ffprobe. Video and audio playback negotiation requires ffprobe even when the original file can be played directly; HLS transcoding additionally requires a compatible FFmpeg encoder build. The `/Items/{id}/File` and `/Download` endpoints remain available without those tools. The server checks its configured tools before advertising playback capabilities.

For the default `scratch` image, supply fully static x86_64 Linux executables and verify them with `scripts/check-static-tools.sh` before using `docker-compose.ffmpeg.yml`. A dynamically linked FFmpeg will not run in that image.

For a dynamic Linux FFmpeg distribution, `Dockerfile.ffmpeg-runtime` and `docker-compose.ffmpeg-runtime.yml` build an operator-assembled local variant from a separate Linux image containing `/usr/bin/ffmpeg`, `/usr/bin/ffprobe`, their runtime libraries, and full package documentation. Its licenses and codec obligations come from the selected external distribution. Retain those notices and keep this combined runtime separate from project releases.

## Development

The server requires a PostgreSQL database, a Rust toolchain, and access to the `web/` directory. The Compose setup is the supported local database path. For a direct development run, set `DATABASE_URL`, `PUFFINBOX_WEB_ROOT`, and `PUFFINBOX_DATA_DIR` for your local environment, then run the project binary. `PUFFINBOX_BIND` is optional and defaults to `127.0.0.1:8096`; set it explicitly only when you intend to listen on another interface. Do not use an existing personal media root for acceptance work.

Linux static builds require musl tools; license generation requires Python 3, cargo-deny and cargo-about. Build `puffinbox-server` before running the Rust tests because media isolation tests re-enter the server's internal worker mode. Project Rust forbids unsafe code across production, tests and examples. Windows development currently uses WSL or Compose; native Windows source builds are unvalidated.

The [GitHub workflows](https://github.com/peppermintish/puffinbox/actions) run source,
dependency, container, and isolated media checks. Release packaging also requires
the documented [release gates](docs/release-gates.json) to pass. Local and
installed-client results are recorded separately in [the validation matrix](docs/compatibility.md).

## License

Puffinbox code is offered under either the MIT License or Apache License 2.0; the full texts are in `LICENSE-MIT` and `LICENSE-APACHE`. The vendored `hls.js` browser player is Apache-2.0 and has separate provenance and attribution in `web/vendor/`.

The default image includes the Cargo dependency license bundle, Rust standard-library and toolchain notices, musl attribution, and the project notices. PostgreSQL is supplied by a separate Compose image. FFmpeg/ffprobe and any associated codec or runtime components are supplied and licensed separately by the operator.

Dependencies use licenses compatible with MIT and Apache-2.0 and retain their own notices. The [licensing inventory](docs/licensing.md) records the accepted terms and distribution boundaries. Release remains gated by current source, container, client, feature, security and scale validation.
