<p align="center">
  <img src="assets/readme-hero.svg" alt="Skyffla" width="100%" />
</p>

<p align="center">
  CLI-first peer communication in Rust with a separate rendezvous service.
</p>

## Install

Add the tap once:

```sh
brew tap skyffla/skyffla
```

Install the CLI:

```sh
brew install skyffla
```

Install the rendezvous server:

```sh
brew install skyffla-rendezvous
```

Only install this if you want to run your own rendezvous service; the CLI uses the public default at `http://rendezvous.skyffla.com`.

## Use

Join a session, or create it if nobody is there yet:

```sh
skyffla join copper-731
```

The first peer hosts the room and waits in the default TUI. The next peer connects with the same command:

```sh
skyffla join copper-731
```

Use explicit host mode when you want deterministic automation:

```sh
skyffla host copper-731
```

The default human surface is room-native:

- plain text sends room broadcast chat
- `/msg <member_id> <text>` sends a direct message
- `/members` shows the live roster
- `/send <member_id|all> <path>` offers a file or folder
- `/save <channel_id> <path>` exports an accepted file or folder

Use the `machine` surface for wrappers and automation:

```sh
skyffla host copper-731 machine
```

```sh
skyffla join copper-731 machine
```

In `machine` mode:

- commands are newline-delimited JSON on `stdin`
- events are newline-delimited JSON on `stdout`
- runtime logs stay on `stderr`

The public machine contract is documented in [`docs/machine-protocol.md`](docs/machine-protocol.md).

Use local discovery on the same LAN without rendezvous. Both peers can use `join --local`:

```sh
skyffla join copper-731 --local
```

```sh
skyffla join copper-731 --local
```

Use `host --local` when you want one side to advertise explicitly:

```sh
skyffla host copper-731 --local
```

```sh
skyffla join copper-731 --local
```

`--local` uses mDNS on the local network, only accepts local peers, and only allows direct p2p connections.

Pipe bytes over stdio:

```sh
printf 'hello\n' | skyffla join copper-731 --stdio
```

```sh
skyffla join copper-731 --stdio
```

Treat the stream ID as a short-lived shared secret. Avoid very short or easy-to-guess IDs; use something less obvious, like `copper-731` instead of `demo`.

In `--stdio` mode, local `stdin` streams to the peer and peer bytes stream to local `stdout`. Status, progress, and errors go to `stderr`.

Capture only the payload:

```sh
skyffla join copper-731 --stdio > received.txt
```

Keep the data stream clean in pipelines:

```sh
printf 'hello\n' | skyffla join copper-731 --stdio 2>sender.log
skyffla join copper-731 --stdio 2>receiver.log | cat
```

`--stdio` is full duplex, so both peers can also send bytes in the same session.

For a simple two-terminal bridge, run this in terminal A:

```sh
cat | skyffla host copper-731 --stdio
```

Run this in terminal B:

```sh
cat | skyffla join copper-731 --stdio
```

Type a line in either terminal and press Enter to send it to the other side. Press `Ctrl-D` to close only your send side; the peer can keep sending until it also reaches EOF.

Use the same pattern on one LAN without rendezvous:

```sh
cat | skyffla host copper-731 --local --stdio
```

```sh
cat | skyffla join copper-731 --local --stdio
```

The CLI defaults to the public rendezvous at `http://rendezvous.skyffla.com:8080`.

Override it for self-hosting with `--server` or `SKYFFLA_RENDEZVOUS_URL`:

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 skyffla join copper-731
```

Run your own rendezvous server:

```sh
skyffla-rendezvous
```

`skyffla-rendezvous` ignores `X-Forwarded-For` by default. Only set
`SKYFFLA_RENDEZVOUS_TRUST_PROXY_HEADERS=true` when it is behind a trusted reverse
proxy that you control.

## Local Dev

Install Rust with `rustup`, then clone and enter the repo:

```sh
git clone git@github.com:skyffla/skyffla.git
cd skyffla
. "$HOME/.cargo/env"
```

Run the main checks:

```sh
cargo test
cargo fmt --check
cargo clippy --workspace --all-targets
```

Run the binaries locally:

```sh
cargo run -p skyffla-rendezvous
```

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 cargo run -p skyffla -- join copper-731
```
