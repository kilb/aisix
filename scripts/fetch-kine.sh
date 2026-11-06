#!/usr/bin/env bash
# Download the pinned kine release binary (linux-amd64) to <dest>, refusing it
# unless its sha256 matches the pin below. This is the one place the pin lives;
# local runs (scripts/cp-test-services.sh) and CI both fetch through here.
#
# The checksum is the one the release publishes in sha256sum-amd64.txt, and
# the digest GitHub computes for the asset.
#
# Usage: scripts/fetch-kine.sh <dest>
set -euo pipefail

KINE_VERSION=v0.17.5
KINE_URL="https://github.com/k3s-io/kine/releases/download/${KINE_VERSION}/kine-amd64"
KINE_SHA256=fc6780ff09301d3e7440c47988d8abbbd5fa380d81ce47ef69d155c82366fd48

dest="${1:?usage: scripts/fetch-kine.sh <dest>}"

if [ "$(uname -s)-$(uname -m)" != "Linux-x86_64" ]; then
  echo "fetch-kine: only the linux-amd64 binary is pinned (this is $(uname -s)-$(uname -m))" >&2
  exit 1
fi

mkdir -p "$(dirname "$dest")"
# Download next to the destination, so the final rename is atomic and a
# refused or partial download never sits at <dest>.
tmp="$(mktemp "${dest}.download.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

curl -fsSL --retry 3 --output "$tmp" "$KINE_URL"
actual="$(sha256sum "$tmp" | cut -d' ' -f1)"
if [ "$actual" != "$KINE_SHA256" ]; then
  echo "fetch-kine: refusing ${KINE_URL}: sha256 is ${actual}, expected ${KINE_SHA256}" >&2
  exit 1
fi

chmod 0755 "$tmp"
mv -f "$tmp" "$dest"
trap - EXIT
echo "fetch-kine: kine ${KINE_VERSION} at ${dest}" >&2
