# Repo Guidance

- Keep docs and examples portable. Use relative paths and repo-relative commands; do not hardcode local absolute paths, personal hostnames, or machine-specific directories.
- Avoid hardcoding GitHub owner names in docs, examples, or scripts unless the value is genuinely required input.
- Treat `skyffla` and `skyffla-rendezvous` as separate deliverables. Packaging, install docs, and release notes should keep their install paths distinct.
- Prefer fast local tooling: use `rg` for search, `cargo test` for verification, and keep checks scoped to the code you changed when possible.
- Keep local-only automation under `scripts/local/` or another gitignored location unless it is meant to ship with the repo.
- Preserve existing user changes. Do not revert unrelated edits, and keep patches focused.
