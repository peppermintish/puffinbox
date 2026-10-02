# Disposable build infrastructure for the experimental runtime probe.
# Neither this builder nor its generated external runtime is a release image.
FROM rust:1.98.1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 binutils perl make \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rust-src

WORKDIR /work
ENTRYPOINT ["python3", "experiments/linux-gnu-runtime/build.py"]
