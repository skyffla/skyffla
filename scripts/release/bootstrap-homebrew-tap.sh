#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/bootstrap-homebrew-tap.sh <github-owner> [tap-suffix]

Example:
  scripts/release/bootstrap-homebrew-tap.sh your-github-user skyffla
EOF
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage >&2
  exit 1
fi

GITHUB_OWNER="$1"
TAP_SUFFIX="${2:-skyffla}"
TAP_REPO="homebrew-${TAP_SUFFIX}"
TAP_REF="${GITHUB_OWNER}/${TAP_SUFFIX}"
TAP_PATH="$(brew --repository "${TAP_REF}")"

if ! command -v brew >/dev/null 2>&1; then
  echo "brew is required to bootstrap the tap repo" >&2
  exit 1
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "gh is required to create the GitHub repository" >&2
  exit 1
fi

if [[ ! -d "${TAP_PATH}/.git" ]]; then
  brew tap-new "${TAP_REF}"
fi

echo "Local tap repo path: ${TAP_PATH}"

if gh auth status -h github.com >/dev/null 2>&1; then
  TAP_GITHUB_REPO="${GITHUB_OWNER}/${TAP_REPO}"
  if gh repo view "${TAP_GITHUB_REPO}" >/dev/null 2>&1; then
    echo "GitHub repo already exists: ${TAP_GITHUB_REPO}"
  else
    gh repo create "${TAP_GITHUB_REPO}" \
      --public \
      --description "Homebrew tap for Skyffla" \
      --source "${TAP_PATH}" \
      --push
  fi
else
  TAP_GITHUB_REPO="${GITHUB_OWNER}/${TAP_REPO}"
  cat <<EOF
GitHub authentication is not ready, so the tap repo was only created locally.

After re-authenticating, run:
  gh auth login -h github.com
  gh repo create ${TAP_GITHUB_REPO} --public --description "Homebrew tap for Skyffla" --source "${TAP_PATH}" --push
EOF
fi
