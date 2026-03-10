# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and the project aims to follow Semantic Versioning.

## [Unreleased]

## [0.2.0] - 2026-03-10

- Add `--local` LAN mode with mDNS discovery so peers on the same network can connect without rendezvous.
- Let `join --local` elect a host automatically when both peers start in join-or-create mode.
- Harden local state writes and add end-to-end coverage for the local discovery flow.

## [0.1.8] - 2026-03-09

- Make `cut-release.sh` wait for the current `main` commit's `CI` run to finish instead of failing fast while the workflow is still queued or running.
- Fail fast on unreadable or corrupted local state instead of silently rotating the local identity or forgetting trust records.
- Treat rendezvous stream deletion as idempotent when the stream has already expired on the server.
- Reject stream IDs that contain special characters with a clear CLI usage error.

## [0.1.7] - 2026-03-08

- Log rendezvous register/resolve/delete requests in a countable format for ops tracking.
- Add a local helper to summarize daily resolve counts from the VM service logs.
- Stop trusting `X-Forwarded-For` in `skyffla-rendezvous` unless explicitly configured for a trusted reverse proxy.
- Clarify the README around less-guessable stream IDs and the safe proxy-header default for self-hosting.

## [0.1.6] - 2026-03-08

- Require a green `CI` run on the current `main` commit before `cut-release.sh` will cut a release.
- Fix a `--stdio` completion race in the join-or-create flow that could fail on Linux CI after payload transfer.

## [0.1.5] - 2026-03-08

- Make `join` claim missing streams so the first peer waits and the next peer connects.
- Restore terminal state more cleanly on exit from the interactive UI.
- Refine the README examples and harden the release helper when `## [Unreleased]` is missing.

## [0.1.4] - 2026-03-08

- Improved Linux release compatibility for Homebrew installs on Debian 12 / Ubuntu 22.04 class systems by lowering the glibc build baseline.
- Restored macOS and Linux ARM release artifacts after validating the Linux x86_64 Homebrew path end to end.
- Simplified the README around install, usage, and local development only.
- Pointed the CLI at the hosted rendezvous server by default while keeping `--server` and `SKYFFLA_RENDEZVOUS_URL` as overrides.

## [0.1.3] - 2026-03-08

- Re-enabled release artifacts for macOS and Linux ARM while keeping Linux builds on the older Ubuntu 22.04 glibc baseline.
- Rewrote the README to focus on Brew installation, core CLI usage, and local development setup.
- Set the default client rendezvous URL to the hosted server at `rendezvous.skyffla.com:8080`.
