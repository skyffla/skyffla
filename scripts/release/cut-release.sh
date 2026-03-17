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
ORIGIN_MAIN_SHA="$(git rev-parse --verify refs/remotes/origin/main 2>/dev/null || true)"
if [[ -z "${ORIGIN_MAIN_SHA}" ]]; then
  echo "origin/main is missing locally; fetch origin before cutting a release" >&2
  exit 1
fi

if [[ "${HEAD_SHA}" != "${ORIGIN_MAIN_SHA}" ]]; then
  echo "HEAD (${HEAD_SHA}) does not match origin/main (${ORIGIN_MAIN_SHA}); push main and wait for CI before releasing" >&2
  exit 1
fi

REPO_SLUG="$(git remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')"
CI_POLL_INTERVAL="${SKYFFLA_RELEASE_CI_POLL_INTERVAL:-10}"
CI_TIMEOUT_SECONDS="${SKYFFLA_RELEASE_CI_TIMEOUT:-1800}"

if [[ ! "${CI_POLL_INTERVAL}" =~ ^[0-9]+$ || "${CI_POLL_INTERVAL}" -le 0 ]]; then
  echo "SKYFFLA_RELEASE_CI_POLL_INTERVAL must be a positive integer" >&2
  exit 1
fi

if [[ ! "${CI_TIMEOUT_SECONDS}" =~ ^[0-9]+$ || "${CI_TIMEOUT_SECONDS}" -le 0 ]]; then
  echo "SKYFFLA_RELEASE_CI_TIMEOUT must be a positive integer" >&2
  exit 1
fi

wait_for_ci_success() {
  local commit_sha="$1"
  local ci_deadline last_ci_state ci_run_line ci_state
  local ci_run_id ci_status ci_conclusion

  ci_deadline="$(( $(date +%s) + CI_TIMEOUT_SECONDS ))"
  last_ci_state=""

  while :; do
    ci_run_line="$(
      gh run list \
        -R "${REPO_SLUG}" \
        --workflow ci.yml \
        --branch main \
        --commit "${commit_sha}" \
        --json databaseId,status,conclusion \
        --jq 'map(select(.databaseId != null))[0] | if . then [.databaseId, .status, (.conclusion // "")] | @tsv else empty end' \
        -L 10
    )"

    if [[ -z "${ci_run_line}" ]]; then
      ci_state="missing"

      if [[ "${ci_state}" != "${last_ci_state}" ]]; then
        echo "waiting for CI to start for ${commit_sha} on main..." >&2
        last_ci_state="${ci_state}"
      fi
    else
      IFS=$'\t' read -r ci_run_id ci_status ci_conclusion <<<"${ci_run_line}"
      ci_state="${ci_run_id}:${ci_status}:${ci_conclusion}"

      if [[ "${ci_state}" != "${last_ci_state}" ]]; then
        echo "CI run ${ci_run_id} status: ${ci_status}${ci_conclusion:+ (${ci_conclusion})}" >&2
        last_ci_state="${ci_state}"
      fi

      if [[ "${ci_status}" == "completed" ]]; then
        if [[ "${ci_conclusion}" == "success" ]]; then
          return 0
        fi

        echo "CI run ${ci_run_id} finished with conclusion: ${ci_conclusion}" >&2
        return 1
      fi
    fi

    if [[ "$(date +%s)" -ge "${ci_deadline}" ]]; then
      echo "timed out after ${CI_TIMEOUT_SECONDS}s waiting for CI on ${commit_sha}" >&2
      return 1
    fi

    sleep "${CI_POLL_INTERVAL}"
  done
}

wait_for_ci_success "${HEAD_SHA}"

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
perl -0pi -e 's/"version": "[^"]+"/"version": "'"${VERSION}"'"/' wrappers/node/package.json
perl -0pi -e 's/__version__ = "[^"]+"/__version__ = "'"${VERSION}"'"/' wrappers/node/src/version.js
perl -0pi -e 's/(\[project\]\n(?:[^\n]*\n)*?version = \")([^\"]+)(\")/${1}'"${VERSION}"'${3}/s' wrappers/python/pyproject.toml
perl -0pi -e 's/__version__ = "[^"]+"/__version__ = "'"${VERSION}"'"/' wrappers/python/src/skyffla/__about__.py
npm install --package-lock-only --prefix wrappers/node >/dev/null
uv lock --project wrappers/python >/dev/null
cargo check >/dev/null

FILES=(
  Cargo.toml
  CHANGELOG.md
  wrappers/node/package.json
  wrappers/node/package-lock.json
  wrappers/node/src/version.js
  wrappers/python/pyproject.toml
  wrappers/python/src/skyffla/__about__.py
  wrappers/python/uv.lock
)
if ! git diff --quiet -- Cargo.lock; then
  FILES+=(Cargo.lock)
fi

git add "${FILES[@]}"
git commit -m "Release ${TAG}"

if [[ "${PUSH}" -eq 1 ]]; then
  git push origin HEAD
  RELEASE_SHA="$(git rev-parse HEAD)"
  wait_for_ci_success "${RELEASE_SHA}"
  git tag "${TAG}"
  git push origin "${TAG}"
else
  git tag "${TAG}"
  cat <<EOF
Created release commit and tag:
  $(git rev-parse --short HEAD) (${TAG})

Push when ready:
  git push origin HEAD
  git push origin ${TAG}
EOF
fi
