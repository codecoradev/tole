# Container image for `tole serve` (issue #1044). Build from the repository root:
#
#   docker build -t tole-serve:local .
#
# Plain HTTP on 7801 inside the container: TLS ends at a reverse proxy (see
# docs/container.md and templates/container/). Base images are pinned by tag AND
# digest; bump both together (the digests are the multi-arch index digests).
#
# BuildKit is REQUIRED: `RUN --mount=type=cache` and `COPY --chmod` are BuildKit
# features. It is the default builder of Docker 23 and later (and of `docker
# buildx`); with an older engine set DOCKER_BUILDKIT=1.

ARG RUST_IMAGE=rust:1.97.0-slim-bookworm@sha256:6d220bf85c74e842a79da63997af8d2e74455c0b8847d8bb3a5888572334991d
ARG RUNTIME_IMAGE=debian:bookworm-20261123-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e

# ---- build: the release binary of tole-cli, default features ----------------
FROM ${RUST_IMAGE} AS build
WORKDIR /src
# The whole workspace is needed: cargo reads every member manifest. .dockerignore
# keeps target/, .git and the other directories out of the context.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# --locked: the build uses exactly the committed Cargo.lock. The cache mounts
# keep the registry and target directory between builds; the binary is copied
# out inside the same RUN because the target mount is not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p tole-cli \
 && install -D -m 0755 target/release/tole /out/tole \
 && strip --strip-all /out/tole \
 && /out/tole --version

# ---- runtime: a small glibc base, a non-root user ---------------------------
FROM ${RUNTIME_IMAGE} AS runtime

# ca-certificates: the provider and web tools use TLS (rustls with the system
# roots). git: the `git` tool and the commands the model runs use it. Nothing
# else; derive an image from this one to add the toolchains a workspace needs.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 tole \
 && useradd --system --uid 10001 --gid tole --home-dir /data/home \
        --no-create-home --shell /usr/sbin/nologin tole \
 && mkdir -p /data/home /data/codecora /data/sessions /workspace \
 && chown -R 10001:10001 /data /workspace \
 && chmod 0700 /data

COPY --from=build /out/tole /usr/local/bin/tole
COPY --chmod=0755 scripts/container/docker-entrypoint.sh scripts/container/healthcheck.sh /usr/local/bin/

# All state is on the two volumes; nothing is written into the image layer.
#   /data       HOME, the CodeCora home (user config, profiles, cron store,
#               discussion logs, trust records) and the session ledgers
#   /workspace  the folder serve is rooted at (--workspace) and the tools work in
# A named volume is created with this ownership on first use; a bind mount keeps
# the host's, so make it writable for uid 10001.
ENV HOME=/data/home \
    CODECORA_HOME=/data/codecora
VOLUME ["/data", "/workspace"]
WORKDIR /workspace
USER 10001:10001

EXPOSE 7801
# GET /health answers 200 without credentials; it answers 503 while serve drains.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/healthcheck.sh"]

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
# The serve token is read from a file (a Docker secret), never from the command
# line. Binding 0.0.0.0 is for the container network only: publish no port for
# this service and put the TLS proxy next to it.
CMD ["tole", "serve", "--bind", "0.0.0.0", "--port", "7801", \
     "--token-file", "/run/secrets/tole_serve_token", \
     "--workspace", "/workspace", "--sessions-dir", "/data/sessions"]
