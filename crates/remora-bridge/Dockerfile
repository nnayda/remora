# Container image for the headless `remora-bridge` (ADR-0021, #234).
# Build context is the workspace root (needs Cargo.lock + the whole workspace
# for `--locked`):
#
#   docker build -f crates/remora-bridge/Dockerfile -t remora-bridge .
#
# Unlike the relay's distroless image, this runtime is debian-slim + an apt
# layer: the bridge shells out to `ssh` (ControlMaster, ADR-0011), which
# distroless cannot provide. The compiled BINARY remains bit-for-bit
# reproducible (same recipe as the relay — see scripts/
# verify-bridge-reproducible.sh); the image DIGEST is not reproducible v1
# because of the apt layer (snapshot.debian.org pinning is a follow-up).
#
# kubectl is deliberately NOT bundled (size + cluster version skew). For
# kubectl transports, extend this image:  FROM remora-bridge  + install the
# kubectl matching your cluster. See crates/remora-bridge/README.md.

# Same digest-pinned builder as the relay (Dependabot refreshes weekly).
FROM rust:1-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p remora-bridge --bins

# Pinned by digest (docker.io/library/debian:bookworm-slim, resolved via
# `docker buildx imagetools inspect debian:bookworm-slim` on 2026-07-06).
# Dependabot keeps this fresh afterwards.
FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-client \
 && rm -rf /var/lib/apt/lists/*

# Non-root runtime user with a real (writable) ~/.ssh: remora-core pins
# ControlPath=~/.ssh/remora-%C (ADR-0011), so multiplexing sockets land
# there. Mount your key and known_hosts as INDIVIDUAL read-only files into
# this directory — never a read-only mount over the whole dir (spec D5).
RUN useradd --uid 65532 --user-group --create-home --home-dir /home/remora remora \
 && mkdir -p /home/remora/.ssh /var/lib/remora-bridge \
 && chmod 700 /home/remora/.ssh /var/lib/remora-bridge \
 && chown remora:remora /home/remora/.ssh /var/lib/remora-bridge

# Land the binary on the default PATH: `docker exec <ctr> remora-bridge …` and
# the k8s exec probes invoke it by bare name (spec D7), and debian-slim's PATH
# does not include `/`, so a `/remora-bridge` copy would be "not found".
COPY --from=build /src/target/release/remora-bridge /usr/local/bin/remora-bridge
USER remora
# Spec D7: bare `docker exec <ctr> remora-bridge status` (and the k8s exec
# liveness probe) must find ctl.sock without flags — the env var carries the
# state dir to every subcommand.
ENV REMORA_BRIDGE_STATE_DIR=/var/lib/remora-bridge
ENTRYPOINT ["remora-bridge", "serve", "/etc/remora/config.toml"]
