# Environment Variables

This page lists environment variables read by Skyffla binaries, wrappers, tests,
and release tooling.

## CLI

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_ROOM_ID` | none | Room ID used when `ROOM_ID` is omitted from `skyffla [OPTIONS] [ROOM_ID]`. A command-line room ID takes precedence. |
| `SKYFFLA_RENDEZVOUS_URL` | `http://rendezvous.skyffla.com:8080` | Rendezvous server used by the CLI. `--server` / `-S` takes precedence. |
| `SKYFFLA_NAME` | none | Peer display name used when `--name` / `-n` is not set. Falls back to `USER`, then `USERNAME`, then `skyffla-peer`. |
| `SKYFFLA_DIRECTORY_TRANSFER_WORKERS` | `4` | Number of parallel workers used for directory transfer entry requests. Invalid or zero values are ignored. |

Skyffla also uses standard environment variables:

| Variable | Purpose |
| --- | --- |
| `USER` | Fallback for the peer display name when `--name` / `-n` and `SKYFFLA_NAME` are not set. |
| `USERNAME` | Final OS fallback for the peer display name when `--name` / `-n`, `SKYFFLA_NAME`, and `USER` are not set. |
| `HOME` | Location for local CLI state, history, legacy auto-accept state, and known-peer files. |

## Rendezvous Server

These variables configure `skyffla-rendezvous`.

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_RENDEZVOUS_ADDR` | `127.0.0.1:8080` | Listen address. |
| `SKYFFLA_RENDEZVOUS_DB_PATH` | `skyffla-rendezvous.db` | SQLite database path. |
| `SKYFFLA_RENDEZVOUS_CLEANUP_INTERVAL_SECONDS` | `30` | Room cleanup interval in seconds. |
| `SKYFFLA_RENDEZVOUS_RATE_LIMIT` | `120` | Maximum requests per rate-limit window. |
| `SKYFFLA_RENDEZVOUS_RATE_WINDOW_SECONDS` | `60` | Rate-limit window length in seconds. |
| `SKYFFLA_RENDEZVOUS_TRUST_PROXY_HEADERS` | `false` | Trust proxy forwarding headers. Truthy values are `1`, `true`, `yes`, and `on`. Only enable behind a trusted proxy. |

## Python And Node Wrappers

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_BIN` | `skyffla` | Binary path used by the Python and Node wrappers, examples, and smoke tests. |
| `SKYFFLA_SKIP_VERSION_CHECK` | unset | Set to `1`, `true`, `yes`, or `on` to bypass wrapper-to-binary version matching. Use only when intentionally testing mixed versions. |

## Local Discovery Test Tuning

These variables are intended for tests and diagnostics around `--local`.

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_LOCAL_JOIN_ELECTION_WINDOW_MS` | `3000` | Local join election window in milliseconds. Invalid or zero values are ignored. |
| `SKYFFLA_LOCAL_HOST_ANNOUNCEMENT_GRACE_MS` | `3000` | Grace period for hearing a local host announcement in milliseconds. Invalid or zero values are ignored. |

## TUI Test Harness

These variables are for non-interactive TUI tests.

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_TUI_SCRIPTED` | unset | Enables scripted TUI mode and bypasses interactive terminal checks. |
| `SKYFFLA_TUI_SCRIPTED_QUIET_PROGRESS` | unset | Suppresses scripted transfer progress lines when set. |

## Performance Tests

These variables tune ignored performance baseline tests.

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_PERF_FILE_MIB` | `512` | Single-file performance test size in MiB. |
| `SKYFFLA_PERF_TREE_MIB` | `512` | Folder-tree performance test total size in MiB. |
| `SKYFFLA_PERF_TREE_FILES` | `128` | Folder-tree performance test file count. |
| `SKYFFLA_PERF_TIMEOUT_SECS` | `180` | Performance test timeout in seconds. |

## Release Tooling

These variables configure local release helper scripts.

| Variable | Default | Purpose |
| --- | --- | --- |
| `SKYFFLA_RELEASE_CI_POLL_INTERVAL` | `10` | Poll interval in seconds for `scripts/release/cut-release.sh` while waiting for CI. |
| `SKYFFLA_RELEASE_CI_TIMEOUT` | `1800` | CI wait timeout in seconds for `scripts/release/cut-release.sh`. |

GitHub release workflows also use repository variables such as
`PUBLISH_PYTHON_PACKAGE=1` and `PUBLISH_NODE_PACKAGE=1` to opt into wrapper
publishing. Those are GitHub repository variables, not variables read by the
Skyffla binaries.
