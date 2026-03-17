#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/sync-example-wrapper-deps.sh <version>

Example:
  scripts/release/sync-example-wrapper-deps.sh 1.2.1
EOF
}

if [[ $# -ne 1 ]]; then
  usage >&2
  exit 1
fi

VERSION="$1"

if [[ ! "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like X.Y.Z" >&2
  exit 1
fi

ROOT_DIR="$(git rev-parse --show-toplevel)"
cd "${ROOT_DIR}"

perl -0pi -e 's/"skyffla": "[^"]+"/"skyffla": "'"${VERSION}"'"/' examples/node/package.json
perl -0pi -e 's/^  "skyffla[^"]*",$/  "skyffla=='"${VERSION}"'",/m' examples/python/pyproject.toml

npm install --package-lock-only --prefix examples/node >/dev/null
uv lock --project examples/python >/dev/null

cat <<EOF
Updated example wrapper dependencies to ${VERSION}:
  examples/node/package.json
  examples/node/package-lock.json
  examples/python/pyproject.toml
  examples/python/uv.lock
EOF
