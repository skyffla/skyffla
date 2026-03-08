<p align="center">
  <img src="assets/readme-hero.svg" alt="Skyffla" width="100%" />
</p>

<p align="center">
  Terminal-first peer communication in Rust with a separate rendezvous service.
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

## Use

Join a session, or create it if nobody is there yet:

```sh
skyffla join demo
```

The first peer waits. The next peer connects with the same command:

```sh
skyffla join demo
```

Use explicit host mode when you want deterministic automation:

```sh
skyffla host demo
```

Pipe bytes over stdio:

```sh
printf 'hello\n' | skyffla join demo --stdio
```

```sh
skyffla join demo --stdio
```

The CLI defaults to the public rendezvous at `http://rendezvous.skyffla.com:8080`.

Override it for self-hosting with `--server` or `SKYFFLA_RENDEZVOUS_URL`:

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 skyffla join demo
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
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 cargo run -p skyffla -- join demo
```
