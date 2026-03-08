# Skyffla CLI

Skyffla is a terminal-native peer communication tool built in Rust.

Current product direction:

- explicit `stream_id` rendezvous
- direct peer connectivity when possible
- relay fallback at the transport layer
- interactive TUI by default
- support for chat, files, folders, clipboard, and `stdio`
- machine-friendly automation mode
- future browser interoperability through a separately negotiated websocket-based transport

The current native transport target for v1 is `iroh`.

## Current Status

The repository is in early implementation.

What exists now:

- Cargo workspace with the planned crate layout
- protocol types and CBOR length-prefixed framing
- session state machine plus minimal runtime event types
- SQLite-backed rendezvous store and `axum` HTTP service
- basic IP-based rendezvous rate limiting
- initial `iroh` transport wrapper with endpoint bootstrap tickets and bidirectional streams
- `skyffla host` and `skyffla join` commands with rendezvous lookup, `Hello/HelloAck`, interactive full-screen terminal UI, text chat, file transfer, tar-based folder transfer, and explicit text clipboard transfer
- `skyffla host --stdio` and `skyffla join --stdio` for pipeline-style byte transport with optional JSON events on `stderr`
- persisted local state in `~/.skyffla/state.json` for command history, known peers, and default auto-accept policy
- per-run acceptance overrides via `--auto-accept` and `--reject-all`
- unit and integration tests for protocol, session, rendezvous, transport, and CLI stdio smoke paths

What does not exist yet:

- richer key-driven TUI navigation beyond the current command-based flow
- persistent transfer history or resumable transfers
- packaging and release automation

## Repository Layout

```text
skyfflacli/
  Cargo.toml
  README.md
  IMPLEMENTATION_PLAN.md
  crates/
    skyffla-cli/
    skyffla-protocol/
    skyffla-rendezvous/
    skyffla-session/
    skyffla-transport/
```

Crate responsibilities:

- `skyffla-cli`: command-line entrypoints, TUI, config, clipboard integration
- `skyffla-protocol`: application protocol types, framing, serialization
- `skyffla-rendezvous`: rendezvous API and stream registry
- `skyffla-session`: session and transfer state machines
- `skyffla-transport`: transport abstraction and `iroh` integration

## Prerequisites

Required on macOS:

- Git
- Rust toolchain
- `tar` available on `PATH` for folder transfer send/extract
- local clipboard access available for `/clip` send/receive

Recommended:

- `rustup` for managing Rust versions and targets
- `clippy` and `rustfmt` via the default Rust profile

## Install Rust

Standard installation uses `rustup`.

```sh
curl -L -sS https://sh.rustup.rs -o /tmp/rustup.sh
sh /tmp/rustup.sh -y --profile default
```

After installation, load Cargo into the current shell:

```sh
. "$HOME/.cargo/env"
```

Verify the toolchain:

```sh
cargo --version
rustc --version
```

Expected result at the time this README was written:

- `cargo 1.94.0`
- `rustc 1.94.0`

If you open a new terminal after installation, this is usually handled automatically by your shell startup files. If `cargo` is still not found, run:

```sh
. "$HOME/.cargo/env"
```

## Getting Started

Clone the repo and enter it:

```sh
git clone <repo-url>
cd skyfflacli
```

If Rust is not already installed, follow the install steps above.

Fetch, build, and test everything:

```sh
. "$HOME/.cargo/env"
cargo test
```

Run formatter and lints:

```sh
. "$HOME/.cargo/env"
cargo fmt
cargo clippy --workspace --all-targets
```

## VS Code Setup

The repo now includes workspace defaults for VS Code in `.vscode/settings.json` and recommended extensions in `.vscode/extensions.json`.

Recommended extensions:

- `rust-lang.rust-analyzer`
- `vadimcn.vscode-lldb`
- `tamasfe.even-better-toml`

Included workspace behavior:

- Rust format-on-save
- TOML format-on-save
- `clippy` as the rust-analyzer check command
- `target/` excluded from file/search views

The repo also pins the Rust toolchain in `rust-toolchain.toml` to `1.94.0` with:

- `rustfmt`
- `clippy`

After opening the workspace in VS Code, install the recommended extensions when prompted.

## Daily Workflow

Typical developer loop:

```sh
. "$HOME/.cargo/env"
cargo test
```

When touching formatting-sensitive code:

```sh
. "$HOME/.cargo/env"
cargo fmt
```

Before opening a PR:

```sh
. "$HOME/.cargo/env"
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test
```

## Release Builds

Build optimized release binaries:

```sh
. "$HOME/.cargo/env"
cargo build --release --bins
```

The resulting executables are:

- `target/release/skyffla`
- `target/release/skyffla-rendezvous`

Check their sizes:

```sh
ls -lh target/release/skyffla target/release/skyffla-rendezvous
```

On the reference macOS arm64 machine used during development, the unstripped release binaries were approximately:

- `skyffla`: `19M`
- `skyffla-rendezvous`: `4.5M`

To strip debug symbols in place:

```sh
strip target/release/skyffla
strip target/release/skyffla-rendezvous
```

To compare exact byte sizes before and after stripping:

```sh
stat -f '%N %z' target/release/skyffla target/release/skyffla-rendezvous
```

These are single executable files, but on macOS they still dynamically link against system frameworks and libraries that are expected to exist on the host system.

## Architecture Notes

The implementation is intentionally split into three layers:

1. Rendezvous layer
2. Session/application layer
3. Transport layer

Design rule:

- the rendezvous API is not the data path after peers connect

Peer/session protocol:

- transport-agnostic
- binary framed control messages
- CBOR-encoded payloads
- explicit transfer lifecycle messages

Transport direction:

- v1 native transport: `iroh`
- future browser transport: separately negotiated relayed websocket transport
- application protocol should not depend on transport-specific semantics

## Important Documents

- Product and delivery plan: `IMPLEMENTATION_PLAN.md`

## Current Commands

Current CLI:

- `skyffla host <stream-id>`
- `skyffla join <stream-id>`

Current supported flags:

- `--server <url>` to point at the rendezvous API
- `--download-dir <path>` to choose where received files are written
- `--name <peer-name>` to set the local handshake name
- `--message <text>` to use a one-shot non-interactive chat send after connect
- `--stdio` to disable the TUI and stream payload bytes over the session
- `--json` to emit machine-readable session events on `stderr`
- `--auto-accept <kinds>` to override acceptance policy for this run only
- `--reject-all` to reject all incoming interactive offers for this run only

Accepted values for `--auto-accept`:

- `file`
- `folder`
- `clipboard`

Example:

```sh
cargo run -p skyffla -- join demo-room --server http://127.0.0.1:18080 --auto-accept file,clipboard
```

Default behavior without `--message`:

- enter a full-screen terminal UI for chat/transfers
- use `/send <path>` to transfer a file or folder
- use `/clip` to transfer text clipboard contents
- use `/accept` or `y` to accept an incoming file offer
- use `/reject` or `n` to reject an incoming file offer
- type `/quit` or `q` to close the session
- transfer status and byte progress update live in the transfer list

Example local smoke test:

Terminal 1:

```sh
. "$HOME/.cargo/env"
SKYFFLA_RENDEZVOUS_ADDR=127.0.0.1:18080 cargo run -p skyffla-rendezvous
```

Terminal 2:

```sh
. "$HOME/.cargo/env"
cargo run -p skyffla -- host demo-room --server http://127.0.0.1:18080 --name host
```

Terminal 3:

```sh
. "$HOME/.cargo/env"
cargo run -p skyffla -- join demo-room --server http://127.0.0.1:18080 --name join
```

Then type chat lines in either terminal and use `/quit` to exit.

File or folder transfer example after connect:

```text
/send /path/to/file.txt
/send /path/to/folder
```

The receiver currently must explicitly `/accept` or `/reject` each incoming file or folder offer.
Shortcuts are also available: `y` accepts, `n` rejects, and `q` quits.
Transfer status and byte progress are shown live in the TUI.
Folder transfer currently shells out to local `tar` on both peers.

Clipboard transfer example after connect:

```text
/clip
```

Clipboard transfer currently supports text only and writes to the local clipboard only after explicit acceptance.

Auto-accept behavior:

- `/autoaccept on` persists a default policy for `file` and `clipboard`
- `/autoaccept off` clears that persisted default
- `folder` is never enabled by `/autoaccept`; use `--auto-accept folder` for a one-run override
- the effective policy is reported at startup
- precedence is:
  1. interactive `/autoaccept` changes during the current session
  2. CLI flags like `--auto-accept` or `--reject-all`
  3. persisted defaults from `~/.skyffla/state.json`
  4. manual acceptance

Current `stdio` example:

Terminal 1:

```sh
. "$HOME/.cargo/env"
SKYFFLA_RENDEZVOUS_ADDR=127.0.0.1:18080 cargo run -p skyffla-rendezvous
```

Terminal 2:

```sh
printf 'hello from stdin\n' | cargo run -p skyffla -- host demo-room --server http://127.0.0.1:18080 --stdio --json
```

Terminal 3:

```sh
cargo run -p skyffla -- join demo-room --server http://127.0.0.1:18080 --stdio --json
```

Current `stdio` behavior:

- host sends stdin bytes to the joiner
- joiner writes payload bytes to stdout
- JSON session/progress events are written to stderr when `--json` is enabled
- clipboard features are not available in `--stdio` mode
- interactive offer acceptance policy flags do not affect `--stdio`; stdio remains its own non-interactive transfer path

Current rendezvous server:

```sh
. "$HOME/.cargo/env"
cargo run -p skyffla-rendezvous
```

Default listen address:

- `127.0.0.1:8080`

Available endpoints:

- `PUT /v1/streams/{stream_id}`
- `GET /v1/streams/{stream_id}`
- `DELETE /v1/streams/{stream_id}`
- `GET /health`

Example registration request:

```sh
curl -X PUT http://127.0.0.1:8080/v1/streams/demo-room \
  -H 'content-type: application/json' \
  -d '{
    "ticket":"bootstrap-ticket",
    "ttl_seconds":600,
    "capabilities":{
      "chat":true,
      "file":true,
      "folder":true,
      "clipboard":true,
      "stdio":true,
      "transport":["native-direct"]
    }
  }'
```

Example lookup:

```sh
curl http://127.0.0.1:8080/v1/streams/demo-room
```

Example delete:

```sh
curl -X DELETE http://127.0.0.1:8080/v1/streams/demo-room
```

Server configuration is environment-variable driven for now:

- `SKYFFLA_RENDEZVOUS_ADDR`
- `SKYFFLA_RENDEZVOUS_DB_PATH`
- `SKYFFLA_RENDEZVOUS_CLEANUP_INTERVAL_SECONDS`
- `SKYFFLA_RENDEZVOUS_RATE_LIMIT`
- `SKYFFLA_RENDEZVOUS_RATE_WINDOW_SECONDS`

## Testing

Current tests cover:

- protocol message validation
- protocol frame encode/decode round trips
- session state transitions
- transfer state transitions
- rendezvous TTL and stream ownership rules
- SQLite-backed rendezvous persistence behavior
- HTTP endpoint behavior and rate limiting
- `iroh` bootstrap ticket round trips
- end-to-end native `iroh` control-stream exchange between two local peers
- CLI config, trust-state, event-shaping, and rendezvous URL unit tests
- binary-level `skyffla --stdio` host/join smoke testing
- manual smoke-test validation for interactive chat and clean `/quit` shutdown
- manual smoke-test validation for `/send` plus `/accept`/`/reject`, transfer progress, and downloaded file contents

Run all tests:

```sh
. "$HOME/.cargo/env"
cargo test
```

Run one crate:

```sh
. "$HOME/.cargo/env"
cargo test -p skyffla-protocol
cargo test -p skyffla-session
cargo test -p skyffla-rendezvous
```

## Next Development Steps

Near-term work:

- connect transport events to `skyffla-session`
- build the full TUI

Expected implementation order:

1. Prove stable end-to-end chat over the session abstraction
2. Add explicit file accept/reject UX and transfer progress
3. Add folder transfer
4. Add clipboard transfer
5. Add `stdio` automation mode
6. Build the TUI on top of the same session events

## Notes For New Contributors

- Keep the session protocol transport-agnostic.
- Do not bake websocket assumptions into application messages.
- Treat relay fallback as a transport concern.
- Prefer small, testable crates over cross-cutting logic in the CLI crate.
- If you add dependencies, keep the workspace lean and justify them.

## Troubleshooting

`cargo: command not found`

Load Cargo in the current shell:

```sh
. "$HOME/.cargo/env"
```

`cargo test` fails while downloading dependencies

- Check network access to `crates.io`
- Retry after loading Cargo into the current shell
- If running in a sandboxed environment, dependency download may need elevated network permissions

`skyffla-rendezvous` starts but requests fail unexpectedly

- Check that `SKYFFLA_RENDEZVOUS_DB_PATH` points to a writable location
- Verify that the chosen listen address is not already in use
- If you are testing rate limiting through a proxy, set `x-forwarded-for` deliberately so requests map to the expected client IP

`rustup` not found

Re-run the install steps in the "Install Rust" section.
