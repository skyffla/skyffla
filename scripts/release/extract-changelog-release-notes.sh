#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/extract-changelog-release-notes.sh <version> [changelog-path]
EOF
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage >&2
  exit 1
fi

VERSION="${1#v}"
CHANGELOG_PATH="${2:-CHANGELOG.md}"

if [[ ! -f "${CHANGELOG_PATH}" ]]; then
  echo "missing changelog: ${CHANGELOG_PATH}" >&2
  exit 1
fi

awk -v version="${VERSION}" '
  $0 ~ "^## \\[" version "\\]" {
    in_section = 1
    found = 1
    next
  }

  in_section && /^## \[/ {
    in_section = 0
    exit
  }

  in_section {
    lines[++count] = $0
  }

  END {
    if (!found) {
      exit 1
    }

    start = 1
    while (start <= count && lines[start] == "") {
      start++
    }

    end = count
    while (end >= start && lines[end] == "") {
      end--
    }

    if (start > end) {
      exit 1
    }

    for (i = start; i <= end; i++) {
      print lines[i]
    }
  }
' "${CHANGELOG_PATH}"
