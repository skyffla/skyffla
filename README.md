# Skyffla

Terminal-first peer communication with a separate rendezvous service.

## Install

Install the CLI:

```sh
brew install skyffla/skyffla/skyffla
```

Install the rendezvous server:

```sh
brew install skyffla/skyffla/skyffla-rendezvous
```

Install both if you want to run the full stack yourself:

```sh
brew install skyffla/skyffla/skyffla
brew install skyffla/skyffla/skyffla-rendezvous
```

## Use

Host a session:

```sh
skyffla host demo
```

Join a session:

```sh
skyffla join demo
```

Pipe bytes over stdio:

```sh
printf 'hello\n' | skyffla host demo --stdio
```

```sh
skyffla join demo --stdio
```

The CLI defaults to the public rendezvous at `http://34.73.17.206:8080`.

Override it for self-hosting with `--server` or `SKYFFLA_RENDEZVOUS_URL`:

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 skyffla host demo
```

Run your own rendezvous server:

```sh
skyffla-rendezvous
```

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
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 cargo run -p skyffla -- host demo
```
