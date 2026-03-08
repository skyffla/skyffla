# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and the project aims to follow Semantic Versioning.

## [Unreleased]

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
