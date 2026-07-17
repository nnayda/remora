# Container image for `remora-relay` (ADR-0021): a blind envelope-frame relay.
# Build context is the workspace root (needs Cargo.lock + the whole workspace
# for `--locked`), so build from repo root:
#
#   docker build -f crates/remora-relay/Dockerfile -t remora-relay .
#
# Multi-stage: compile in a full `rust:*-bookworm` build image, run in
# Google's distroless `cc-debian12` (glibc + libgcc + no shell, no package
# manager) as an unprivileged user. The relay binary this produces is
# bit-for-bit reproducible; see crates/remora-relay/README.md ("Reproducible
# builds") for how to verify it and for reproducing the image digest. The
# digest pins below are refreshed weekly by Dependabot (.github/dependabot.yml).

# Pinned by digest (docker.io/library/rust:1-bookworm, resolved via
# `docker buildx imagetools inspect rust:1-bookworm` on 2026-07-02). The
# `1-bookworm` tag tracks the current Rust "stable" channel, matching this
# repo's floating `rust-toolchain.toml` (`channel = "stable"`). Dependabot
# refreshes this digest weekly so the build image doesn't silently drift behind
# a new stable release.
FROM rust:1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p remora-relay

# Pinned by digest (gcr.io/distroless/cc-debian12:latest, resolved via
# `docker buildx imagetools inspect gcr.io/distroless/cc-debian12` on
# 2026-07-02). Distroless images ship both a root and a preconfigured
# `nonroot` user (uid 65532) regardless of tag suffix; USER below selects it.
FROM gcr.io/distroless/cc-debian12@sha256:7ee09f36862efbdbf70422db263e411c2618409ca46faa555bd5b636155307df
COPY --from=build /src/target/release/remora-relay /remora-relay
USER nonroot
# Config path is fixed; mount your relay.toml at this path (see README).
ENTRYPOINT ["/remora-relay", "/etc/remora/relay.toml"]
