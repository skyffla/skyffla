#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/cut-release.sh [<version>] [--push]

Examples:
  scripts/release/cut-release.sh
  scripts/release/cut-release.sh 0.1.4
  scripts/release/cut-release.sh 0.1.4 --push
EOF
}

if [[ $# -gt 2 ]]; then
  usage >&2
  exit 1
fi

PUSH=0
VERSION=""

for arg in "$@"; do
  if [[ "${arg}" == "--push" ]]; then
    PUSH=1
  elif [[ -z "${VERSION}" ]]; then
    VERSION="${arg}"
  else
    usage >&2
    exit 1
  fi
done

if [[ -n "${VERSION}" && ! "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like X.Y.Z" >&2
  exit 1
fi

ROOT_DIR="$(git rev-parse --show-toplevel)"
cd "${ROOT_DIR}"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree must be clean before cutting a release" >&2
  exit 1
fi

CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "${CURRENT_BRANCH}" != "main" ]]; then
  echo "releases must be cut from main (current branch: ${CURRENT_BRANCH})" >&2
  exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI is required to verify CI status before releasing" >&2
  exit 1
fi

HEAD_SHA="$(git rev-parse HEAD)"
REPO_SLUG="$(git remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\\.git$##')"
CI_RUN_ID="$(
  gh api \
    "repos/${REPO_SLUG}/actions/workflows/ci.yml/runs" \
    -f branch=main \
    -f event=push \
    -f head_sha="${HEAD_SHA}" \
    -f per_page=20 \
    --jq '.workflow_runs[] | select(.status == "completed" and .conclusion == "success") | .id' \
    | head -n 1
)"

if [[ -z "${CI_RUN_ID}" ]]; then
  echo "no successful CI run found for ${HEAD_SHA} on main; push main and wait for CI to pass before releasing" >&2
  exit 1
fi

TODAY="$(date +%F)"

if ! grep -q '^## \[Unreleased\]$' CHANGELOG.md; then
  perl -0pi -e 's/(The format is based on Keep a Changelog and the project aims to follow Semantic Versioning\.\n)/${1}\n## [Unreleased]\n/s' CHANGELOG.md
fi

CURRENT_VERSION="$(
  awk '
    $0 == "[workspace.package]" { in_section = 1; next }
    /^\[/ && in_section { exit }
    in_section && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
  ' Cargo.toml
)"

if [[ -z "${CURRENT_VERSION}" ]]; then
  echo "failed to read workspace version from Cargo.toml" >&2
  exit 1
fi

if [[ -z "${VERSION}" ]]; then
  IFS=. read -r major minor patch <<<"${CURRENT_VERSION}"
  VERSION="${major}.${minor}.$((patch + 1))"
fi

TAG="v${VERSION}"

if git rev-parse --verify --quiet "${TAG}" >/dev/null; then
  echo "tag already exists: ${TAG}" >&2
  exit 1
fi

if [[ "${CURRENT_VERSION}" == "${VERSION}" ]]; then
  echo "workspace version is already ${VERSION}" >&2
  exit 1
fi

if ! grep -q "^## \\[${VERSION}\\]" CHANGELOG.md; then
  if ! awk '
    $0 == "## [Unreleased]" { in_section = 1; next }
    in_section && /^## \[/ { exit found ? 0 : 1 }
    in_section && /^- / { found = 1 }
    END { exit in_section && found ? 0 : 1 }
  ' CHANGELOG.md; then
    echo "CHANGELOG.md has no unreleased entries to cut into ${VERSION}" >&2
    exit 1
  fi

  perl -0pi -e "s/^## \\[Unreleased\\]\\n/## [Unreleased]\\n\\n## [${VERSION}] - ${TODAY}\\n/m" CHANGELOG.md
fi

perl -0pi -e 's/(\[workspace\.package\]\n(?:[^\n]*\n)*?version = \")([^\"]+)(\")/${1}'"${VERSION}"'${3}/s' Cargo.toml

cargo check >/dev/null

FILES=(Cargo.toml CHANGELOG.md)
if ! git diff --quiet -- Cargo.lock; then
  FILES+=(Cargo.lock)
fi

git add "${FILES[@]}"
git commit -m "Release ${TAG}"
git tag "${TAG}"

if [[ "${PUSH}" -eq 1 ]]; then
  git push origin HEAD
  git push origin "${TAG}"
else
  cat <<EOF
Created release commit and tag:
  $(git rev-parse --short HEAD) (${TAG})

Push when ready:
  git push origin HEAD
  git push origin ${TAG}
EOF
fi
