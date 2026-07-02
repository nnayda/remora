#!/usr/bin/env bash
# Verify the remora-relay container build is bit-for-bit reproducible.
#
# Builds crates/remora-relay/Dockerfile twice from a clean cache, extracts the
# compiled binary from each image, and asserts the two binaries have an
# identical sha256. Exits non-zero (with a byte-offset report) on mismatch.
#
# This checks the *binary* — the artifact operators actually run. The image
# *digest* additionally depends on layer timestamps and needs an extra flag to
# reproduce; see the "Reproducible builds" section of
# crates/remora-relay/README.md for that recipe. The binary is the meaningful
# guarantee and is what CI regression-checks.
#
#   ./scripts/verify-relay-reproducible.sh
#
# Requires: docker, sha256sum. Run from anywhere; paths are resolved relative
# to the repo root.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dockerfile="crates/remora-relay/Dockerfile"
tag_a="remora-relay:repro-a"
tag_b="remora-relay:repro-b"
work="$(mktemp -d)"

cleanup() {
  rm -rf "$work"
  docker rmi -f "$tag_a" "$tag_b" >/dev/null 2>&1 || true
}
trap cleanup EXIT

extract() {
  # $1 = image tag, $2 = output path — copy /remora-relay out of the image.
  local cid
  cid="$(docker create "$1")"
  docker cp "$cid:/remora-relay" "$2"
  docker rm "$cid" >/dev/null
}

echo "==> build A (clean cache)"
docker build --no-cache -f "$repo_root/$dockerfile" -t "$tag_a" "$repo_root"
extract "$tag_a" "$work/relay-a"

echo "==> build B (clean cache)"
docker build --no-cache -f "$repo_root/$dockerfile" -t "$tag_b" "$repo_root"
extract "$tag_b" "$work/relay-b"

sum_a="$(sha256sum "$work/relay-a" | cut -d' ' -f1)"
sum_b="$(sha256sum "$work/relay-b" | cut -d' ' -f1)"

echo
echo "binary A: $sum_a"
echo "binary B: $sum_b"

if [ "$sum_a" = "$sum_b" ]; then
  echo "REPRODUCIBLE: the relay binary is bit-for-bit identical across builds."
else
  echo "NOT REPRODUCIBLE: the relay binary differs between builds." >&2
  cmp "$work/relay-a" "$work/relay-b" || true
  exit 1
fi
