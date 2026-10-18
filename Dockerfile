# syntax=docker/dockerfile:1.7
#
# costerra container image.
#
#   docker build -t costerra .
#   docker run --rm -v "$PWD:/work" costerra breakdown . --offline
#
# Cargo's registry and target directory live in BuildKit cache mounts, not in
# image layers: rebuilds are incremental and nothing is left in the image
# (reclaim the cache with `docker builder prune`).

ARG RUST_VERSION=1.98
ARG DEBIAN_RELEASE=trixie
# Pinned by digest (multi-arch index); bump with
#   docker buildx imagetools inspect cgr.dev/chainguard/wolfi-base:latest
ARG RUNTIME_IMAGE=cgr.dev/chainguard/wolfi-base:latest@sha256:256c88bb066424045021090932d50be93c8e3f2ce3586707702a401b146f3e43

FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY data data
COPY src src
RUN --mount=type=cache,id=costerra-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=costerra-target,target=/src/target,sharing=locked \
    cargo build --release --locked \
    && install -m 0755 target/release/costerra /usr/local/bin/costerra

# Wolfi (glibc, minimal, packages patched quickly): far fewer CVEs than a
# general-purpose distribution base.
FROM ${RUNTIME_IMAGE}
# git: `costerra diff --base <ref>` and remote modules; ca-certificates: HTTPS;
# curl + jq: posting merge request comments from CI (ci/gitlab-ci.yml).
RUN apk upgrade --no-cache \
    && apk add --no-cache ca-certificates-bundle git curl jq \
    && git config --system --add safe.directory '*'
COPY --from=build /usr/local/bin/costerra /usr/local/bin/costerra
# Non-root user of the base image (uid 65532).
USER nonroot
ENV HOME=/home/nonroot COSTERRA_CACHE_DIR=/home/nonroot/.cache/costerra
WORKDIR /work
# A command-line tool, not a service: nothing to health-check.
HEALTHCHECK NONE
ENTRYPOINT ["costerra"]
CMD ["--help"]

ARG SOURCE_URL=https://github.com/echaouchna/costerra
LABEL org.opencontainers.image.title="costerra" \
      org.opencontainers.image.description="Fast cost estimates and optimization insights for Google Cloud Terraform & Terragrunt" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.source="${SOURCE_URL}"
