#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/render-homebrew-formula.sh \
    <formula-class> <binary-name> <description> \
    <version> <github-owner> <source-repo> \
    <darwin-arm64-sha256> <darwin-x86_64-sha256> \
    <linux-x86_64-sha256> <linux-arm64-sha256>
EOF
}

if [[ $# -ne 10 ]]; then
  usage >&2
  exit 1
fi

FORMULA_CLASS="$1"
BINARY_NAME="$2"
DESCRIPTION="$3"
VERSION="$4"
GITHUB_OWNER="$5"
SOURCE_REPO="$6"
DARWIN_ARM64_SHA="$7"
DARWIN_X86_64_SHA="$8"
LINUX_X86_64_SHA="$9"
LINUX_ARM64_SHA="${10}"
TAG="v${VERSION}"
ASSET_PREFIX="https://github.com/${GITHUB_OWNER}/${SOURCE_REPO}/releases/download/${TAG}"

cat <<EOF
class ${FORMULA_CLASS} < Formula
  desc "${DESCRIPTION}"
  homepage "https://github.com/${GITHUB_OWNER}/${SOURCE_REPO}"
  version "${VERSION}"
  license "MIT"

  if OS.mac?
    if Hardware::CPU.arm?
      url "${ASSET_PREFIX}/skyffla-v${VERSION}-aarch64-apple-darwin.tar.gz"
      sha256 "${DARWIN_ARM64_SHA}"
    else
      url "${ASSET_PREFIX}/skyffla-v${VERSION}-x86_64-apple-darwin.tar.gz"
      sha256 "${DARWIN_X86_64_SHA}"
    end
  elsif OS.linux?
    if Hardware::CPU.arm?
      url "${ASSET_PREFIX}/skyffla-v${VERSION}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "${LINUX_ARM64_SHA}"
    else
      url "${ASSET_PREFIX}/skyffla-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${LINUX_X86_64_SHA}"
    end
  end

  def install
    bin.install "${BINARY_NAME}"
    prefix.install_metafiles
  end

  test do
    if "${BINARY_NAME}" == "skyffla"
      assert_match "Usage:", shell_output("#{bin}/skyffla --help")
    else
      port = free_port
      db_path = testpath/"skyffla-rendezvous.db"
      pid = fork do
        ENV["SKYFFLA_RENDEZVOUS_ADDR"] = "127.0.0.1:#{port}"
        ENV["SKYFFLA_RENDEZVOUS_DB_PATH"] = db_path.to_s
        exec bin/"skyffla-rendezvous"
      end

      begin
        sleep 2
        assert_match "ok", shell_output("curl -fsS http://127.0.0.1:#{port}/health")
      ensure
        begin
          Process.kill("TERM", pid)
        rescue Errno::ESRCH
          nil
        end
        Process.wait(pid)
      end
    end
  end
end
EOF
