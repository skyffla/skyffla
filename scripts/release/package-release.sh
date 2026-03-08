#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/package-release.sh <version> <target-triple> <bin-dir> [out-dir]
EOF
}

if [[ $# -lt 3 || $# -gt 4 ]]; then
  usage >&2
  exit 1
fi

VERSION="$1"
TARGET_TRIPLE="$2"
BIN_DIR="$3"
OUT_DIR="${4:-dist}"
ARCHIVE_NAME="skyffla-v${VERSION}-${TARGET_TRIPLE}.tar.gz"

if [[ ! -x "${BIN_DIR}/skyffla" ]]; then
  echo "missing binary: ${BIN_DIR}/skyffla" >&2
  exit 1
fi

if [[ ! -x "${BIN_DIR}/skyffla-rendezvous" ]]; then
  echo "missing binary: ${BIN_DIR}/skyffla-rendezvous" >&2
  exit 1
fi

mkdir -p "${OUT_DIR}"

STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/skyffla-package.XXXXXX")"
cleanup() {
  rm -rf "${STAGE_DIR}"
}
trap cleanup EXIT

install -m 0755 "${BIN_DIR}/skyffla" "${STAGE_DIR}/skyffla"
install -m 0755 "${BIN_DIR}/skyffla-rendezvous" "${STAGE_DIR}/skyffla-rendezvous"
install -m 0644 README.md "${STAGE_DIR}/README.md"
install -m 0644 LICENSE "${STAGE_DIR}/LICENSE"

tar -C "${STAGE_DIR}" -czf "${OUT_DIR}/${ARCHIVE_NAME}" \
  skyffla \
  skyffla-rendezvous \
  README.md \
  LICENSE

echo "${OUT_DIR}/${ARCHIVE_NAME}"
