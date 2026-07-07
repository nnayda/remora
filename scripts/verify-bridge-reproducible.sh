#!/usr/bin/env bash
# Verify the remora-bridge container build is bit-for-bit reproducible.
#
# Builds crates/remora-bridge/Dockerfile twice from a clean cache, extracts the
# compiled binary from each image, and asserts the two binaries have an
# identical sha256. Exits non-zero (with a byte-offset report) on mismatch.
#
# This checks the *binary* — the artifact operators actually run. The image
# *digest* additionally depends on layer timestamps and needs an extra flag to
# reproduce; see the "Reproducible builds" section of
# crates/remora-bridge/README.md for that recipe. The binary is the meaningful
# guarantee and is what CI regression-checks.
#
#   ./scripts/verify-bridge-reproducible.sh
#
# Requires: docker, sha256sum. Run from anywhere; paths are resolved relative
# to the repo root.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dockerfile="crates/remora-bridge/Dockerfile"
tag_a="remora-bridge:repro-a"
tag_b="remora-bridge:repro-b"
work="$(mktemp -d)"

cleanup() {
  rm -rf "$work"
  docker rmi -f "$tag_a" "$tag_b" >/dev/null 2>&1 || true
}
trap cleanup EXIT

extract() {
  # $1 = image tag, $2 = output path — copy /remora-bridge out of the image.
  local cid
  cid="$(docker create "$1")"
  docker cp "$cid:/remora-bridge" "$2"
  docker rm "$cid" >/dev/null
}

echo "==> build A (clean cache)"
docker build --no-cache -f "$repo_root/$dockerfile" -t "$tag_a" "$repo_root"
extract "$tag_a" "$work/bridge-a"

echo "==> build B (clean cache)"
docker build --no-cache -f "$repo_root/$dockerfile" -t "$tag_b" "$repo_root"
extract "$tag_b" "$work/bridge-b"

sum_a="$(sha256sum "$work/bridge-a" | cut -d' ' -f1)"
sum_b="$(sha256sum "$work/bridge-b" | cut -d' ' -f1)"

echo
echo "binary A: $sum_a"
echo "binary B: $sum_b"

if [ "$sum_a" = "$sum_b" ]; then
  echo "REPRODUCIBLE: the bridge binary is bit-for-bit identical across builds."
else
  echo "NOT REPRODUCIBLE: the bridge binary differs between builds." >&2
  cmp "$work/bridge-a" "$work/bridge-b" || true
  exit 1
fi
