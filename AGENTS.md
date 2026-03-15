# Repo Guidance

- Keep docs and examples portable. Use relative paths and repo-relative commands; do not hardcode local absolute paths, personal hostnames, or machine-specific directories.
- Avoid hardcoding GitHub owner names in docs, examples, or scripts unless the value is genuinely required input.
- Treat `skyffla` and `skyffla-rendezvous` as separate deliverables. Packaging, install docs, and release notes should keep their install paths distinct.
- Prefer fast local tooling: use `rg` for search, `cargo test` for verification, and keep checks scoped to the code you changed when possible.
- Keep the default test lane fast and honest:
  - prefer targeted `cargo test -p ...` runs for the code you changed before reaching for the full workspace or package suite
  - do not hide flaky or slow coverage by deleting it; move it into an explicit slow lane (`#[ignore]`, dedicated script, or documented follow-up command) and keep at least one stable smoke path in the default suite
  - avoid running multiple `cargo test` commands against the same crate in parallel unless there is a specific reason; process-heavy integration tests in this repo contend on cargo locks and local discovery resources
  - when a test is slow because of production timing margins, prefer test-only timing overrides or smaller harness timeouts over weakening assertions
  - if a test stays flaky, fix the race or isolate the flaky scenario explicitly; do not normalize retry-until-green behavior in the default path
- Keep local-only automation under `scripts/local/` or another gitignored location unless it is meant to ship with the repo.
- Preserve existing user changes. Do not revert unrelated edits, and keep patches focused.
