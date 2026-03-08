# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and the project aims to follow Semantic Versioning.

## [Unreleased]

- Improved Linux release compatibility for Homebrew installs on Debian 12 / Ubuntu 22.04 class systems by lowering the glibc build baseline.
- Temporarily narrowed the Homebrew release matrix to `x86_64 Linux` while validating the first end-to-end public install flow.

## [0.1.2] - 2026-03-08

- Rebuilt Linux release artifacts on Ubuntu 22.04 and limited the Brew release to `x86_64 Linux` so the first public install path works on Debian 12.
