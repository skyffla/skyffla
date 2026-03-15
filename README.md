<p align="center">
  <img src="assets/readme-hero.svg" alt="Skyffla" width="100%" />
</p>

<p align="center">
  Multi-party peer-to-peer rooms for terminals, agents, and scripts.
</p>

Skyffla is a room-first, multi-party peer-to-peer system.

Think of it as a practical mesh-oriented room layer:

- multi-party rooms are the default
- 1:1 is just the smallest room
- once a room is established, member traffic goes peer-to-peer instead of through a central chat relay

Its native model is:

- one room
- one host-owned room authority
- any number of members
- direct peer-to-peer chat and payload traffic once the room is established

The primary room surfaces are:

- a room-native TUI for people
- a framed `machine` protocol for wrappers and automation

It also includes raw full-duplex `--stdio` for 1:1 byte streams when you do not
want room semantics.

Use it when you want direct peer-to-peer communication without building your own
room control, peer introduction, file transfer, and terminal UX stack first.

## Why Skyffla

- Multi-party is native. A room is the product abstraction; 1:1 is just a room with two members.
- Mesh-oriented where it matters. Room authority is host-owned, but chat and payload traffic are peer-to-peer between members.
- Direct by default. Chat, file/folder transfer, and channel traffic stay peer-to-peer once a room is established.
- One room model across human and machine surfaces. The TUI and `machine` speak the same room protocol.
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

Open two or more terminals and join the same room.

Terminal A:

```sh
skyffla join copper-731
```

Terminal B:

```sh
skyffla join copper-731
```

The first peer hosts the room. Every later peer joins that same room.

You can keep adding members:

```sh
skyffla join copper-731
```

In the default TUI:

- plain text sends room chat
- `/msg <member>` sends a direct message to one member
- `/send <member|all> <path>` sends a file or folder
- `/members` shows the live roster
- `/quit` leaves the room

Treat the room ID as a short-lived shared secret. Prefer something less obvious
than `demo`.

## Room Surfaces

### `machine`

Use `machine` for wrappers, agents, and scripted room automation:

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

Both the default TUI and `machine` are room-native:

- they join the same kind of multi-party room
- they see the same room/member/channel concepts
- they differ only in presentation and automation surface

## Raw Duplex `--stdio`

Use `--stdio` when you want a raw full-duplex 1:1 byte pipe instead of room
chat or the room-native `machine` API.

`--stdio` is useful for:

- shell pipelines
- agent-to-agent byte streams
- custom framed protocols such as NDJSON

It is not the multiparty room API.

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

If you want wrappers or automation to participate in rooms, use `machine`, not
`--stdio`.

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
- [`docs/iroh-infra-notes.md`](docs/iroh-infra-notes.md): self-hosting guidance for rendezvous, relays, and blob transfer infrastructure
- [`docs/stdio-duplex-spec.md`](docs/stdio-duplex-spec.md): raw duplex `--stdio` design note
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
