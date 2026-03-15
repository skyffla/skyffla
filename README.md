<p align="center">
  <img src="assets/readme-hero.svg" alt="Skyffla" width="100%" />
</p>

<p align="center">
  Peer-to-peer rooms for terminals, agents, and scripts.
</p>

Skyffla gives you one direct room model through three surfaces:

- a room-native TUI for people
- a framed `machine` protocol for wrappers and automation
- raw full-duplex `--stdio` for byte streams

Use it when you want direct peer-to-peer communication without building your own
signaling, room control, file transfer, and terminal UX stack first.

## Why Skyffla

- Direct by default. Chat, file/folder transfer, and channel traffic stay peer-to-peer once a room is established.
- One mental model. Humans, wrappers, and raw pipelines all join the same kind of room.
- Easy to adopt. Use the public rendezvous by default, or self-host `skyffla-rendezvous` when you need control.

## Install

Add the tap once:

```sh
brew tap skyffla/skyffla
```

Install the CLI:

```sh
brew install skyffla
```

Install the rendezvous server only if you want to self-host discovery:

```sh
brew install skyffla-rendezvous
```

By default, the CLI uses the public rendezvous at `http://rendezvous.skyffla.com:8080`.

## Quick Start

Open two terminals and join the same room.

Terminal A:

```sh
skyffla join copper-731
```

Terminal B:

```sh
skyffla join copper-731
```

The first peer hosts the room. The next peer joins it.

In the default TUI:

- plain text sends room chat
- `/msg <member>` sends a direct message
- `/send <member|all> <path>` sends a file or folder
- `/members` shows the live roster
- `/quit` leaves the room

Treat the room ID as a short-lived shared secret. Prefer something less obvious
than `demo`.

## Other Surfaces

### `machine`

Use `machine` for wrappers, agents, and scripted automation:

```sh
skyffla host copper-731 machine
```

```sh
skyffla join copper-731 machine
```

In `machine` mode:

- commands go to `stdin`
- events come from `stdout`
- diagnostics stay on `stderr`

Skyffla accepts either newline-delimited JSON or the convenience slash commands
documented for the CLI runtime. The public wrapper-facing contract is in
[`docs/machine-protocol.md`](docs/machine-protocol.md).

### `--stdio`

Use `--stdio` when you want a raw full-duplex byte pipe instead of room chat or
structured machine events.

Terminal A:

```sh
cat | skyffla host copper-731 --stdio
```

Terminal B:

```sh
cat | skyffla join copper-731 --stdio
```

Type in either terminal to send bytes to the other. `Ctrl-D` closes only your
send side.

### `--local`

Use `--local` on one LAN when you do not want rendezvous:

```sh
skyffla join copper-731 --local
```

```sh
skyffla join copper-731 --local
```

Or make one side the explicit host:

```sh
skyffla host copper-731 --local
```

`--local` uses mDNS discovery, only accepts local peers, and only allows direct
peer-to-peer connections.

## Self-Hosting Rendezvous

Run your own rendezvous server:

```sh
skyffla-rendezvous
```

Point the CLI at it:

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 skyffla join copper-731
```

`skyffla-rendezvous` ignores `X-Forwarded-For` by default. Only set
`SKYFFLA_RENDEZVOUS_TRUST_PROXY_HEADERS=true` when it is behind a trusted proxy
that you control.

## Docs

- [`docs/room-architecture.md`](docs/room-architecture.md): room-native architecture and design boundaries
- [`docs/machine-protocol.md`](docs/machine-protocol.md): wrapper-facing `machine` contract
- [`docs/versioning.md`](docs/versioning.md): compatibility rules for wire, machine, and rendezvous protocols

## Local Development

With Rust installed:

```sh
cargo build --bins
```

```sh
cargo test -p skyffla
```

Run the rendezvous server locally:

```sh
cargo run -p skyffla-rendezvous
```

Join a room against that local server:

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 cargo run -p skyffla -- join copper-731
```
