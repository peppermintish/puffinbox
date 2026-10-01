FROM rust:1.98.1-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools python3 binutils perl make \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install --locked --version 0.20.2 cargo-deny \
    && cargo install --locked --features cli --version 0.9.2 cargo-about \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
COPY src ./src
COPY migrations ./migrations

RUN CARGO_BUILD_JOBS=2 cargo build --locked --release --target x86_64-unknown-linux-musl --bin puffinbox-server

COPY web ./web
COPY docs ./docs
COPY scripts ./scripts
COPY LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES.md ./
COPY about.toml deny.toml third-party.hbs ./

RUN python3 scripts/build_license_bundle.py \
    && mkdir -p /out/data /out/tmp \
    && chown 10001:10001 /out/data /out/tmp \
    && install -m 0555 target/x86_64-unknown-linux-musl/release/puffinbox-server /out/puffinbox-server \
    && if readelf -l /out/puffinbox-server | grep -q 'INTERP'; then echo 'server has a dynamic program interpreter' >&2; exit 1; fi \
    && if readelf -d /out/puffinbox-server 2>&1 | grep -q '(NEEDED)'; then echo 'server has dynamic library dependencies' >&2; exit 1; fi

FROM scratch

COPY --from=build --chown=0:0 /out/puffinbox-server /puffinbox-server
COPY --from=build --chown=0:0 /src/web /app/web
COPY --from=build --chown=0:0 /src/docs /app/docs
COPY --from=build --chown=0:0 /src/dist/licenses /licenses
COPY --from=build --chown=10001:10001 /out/data /data
COPY --from=build --chown=10001:10001 /out/tmp /tmp

USER 10001:10001
WORKDIR /app
ENV PUFFINBOX_WEB_ROOT=/app/web \
    PUFFINBOX_DATA_DIR=/data
EXPOSE 8096
ENTRYPOINT ["/puffinbox-server"]
