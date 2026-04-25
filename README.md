<p align="center">
  <img src="assets/readme-hero.svg" alt="Skyffla" width="100%" />
</p>

# Multi-party peer-to-peer rooms for terminals, agents, and scripts

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

- a room-native Terminal UI (TUI) for people
- a framed `machine` protocol for wrappers and automation

Use it when you want direct peer-to-peer communication without building your own
room control, peer introduction, file transfer, and terminal UX stack first.

## Why Skyffla

- Multi-party is native. A room is the product abstraction; 1:1 is just a room with two members.
- Mesh-oriented where it matters. Room authority is host-owned, but chat and payload traffic are peer-to-peer between members.
- Direct by default. Chat, file/folder transfer, and channel traffic stay peer-to-peer once a room is established.
- One room model across human and machine surfaces. The Terminal UI (TUI) and `machine` speak the same room protocol.
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

Check the installed CLI version:

```sh
skyffla --version
```

Linux release tarballs are published as static `*-unknown-linux-musl` builds so
they run on a wider range of distros, including older WSL Ubuntu images that do
not provide the `glibc` version from the GitHub Actions runner.

## Quick Start

Open two or more terminals and join the same room.

Terminal A:

```sh
skyffla copper-731
```

Terminal B:

```sh
skyffla copper-731
```

The first peer hosts the room. Every later peer joins that same room.

You can keep adding members:

```sh
skyffla copper-731
```

In the default TUI:

- plain text sends room chat
- `/msg <member>` sends a direct message to one member
- `/send <member|all> <path>` sends a file or folder
- `/members` shows the live roster
- `/quit` leaves the room

Treat the room ID as a short-lived shared secret. Prefer something less obvious
than `demo`.

## CLI Options

```sh
skyffla [OPTIONS] [ROOM_ID]
```

`ROOM_ID` can also come from `SKYFFLA_ROOM_ID`. A room ID passed on the command
line wins over the environment variable.

| Short | Long | Purpose |
| --- | --- | --- |
| `-H` | `--host` | Explicitly host the room instead of join-or-promote |
| `-m` | `--machine` | Use the machine protocol instead of the default TUI |
| `-s <path>` | `--send <path>` | Stay online and send a file or folder to each room member once; use `-` to read a finite one-shot file payload from stdin |
|  | `--as <name>` | Receiver-facing transfer name; required when `--send` is `-` |
| `-r` | `--receive` | Stay online and auto-accept incoming file or folder transfers, saving them to `--download-dir` unless `--output` is set |
|  | `--output <path>` | Receive output destination; use `-` with `--receive` to write one received file payload to stdout |
| `-c` | `--send-clipboard` | Stay online and send local clipboard text changes to room members |
| `-C` | `--receive-clipboard` | Stay online and apply incoming clipboard text updates locally |
| `-S <url>` | `--server <url>` | Use a rendezvous server instead of the default public server |
| `-d <path>` | `--download-dir <path>` | Save accepted transfers in this directory |
| `-n <name>` | `--name <name>` | Set the display name for this peer; overrides `SKYFFLA_NAME` |
| `-j` | `--json` | Emit machine events as JSON |
| `-l` | `--local` | Use LAN-only mDNS discovery instead of rendezvous |
| `-a` | `--auto-accept` | Auto-accept incoming file, folder, and clipboard channels in TUI or machine mode |
| `-R` | `--reject-all` | Reject incoming channels by default |
| `-h` | `--help` | Print help |
| `-V` | `--version` | Print version |

`--send`, `--receive`, `--send-clipboard`, and `--receive-clipboard` are
automation modes and are mutually exclusive. They already manage the machine
runtime, logging, and transfer acceptance policy, so do not combine them with
`--machine`, `--json`, `--auto-accept`, or `--reject-all`.

Use `--send - --as <name>` when a pipeline produces the payload. Stdin sends
are one-shot: after at least one peer receives or rejects the payload, the sender
leaves the room instead of staying online for late joiners.

```sh
tar -czf - logs/ | skyffla copper-731 --send - --as logs.tgz
```

Use `--receive --output -` to write one incoming file payload to stdout. Status
logs go to stderr in this mode.

```sh
skyffla copper-731 --receive --output - > logs.tgz
```

## Room Surfaces

### `machine`

Use `machine` for wrappers, agents, and scripted room automation:

```sh
skyffla copper-731 --host --machine
```

```sh
skyffla copper-731 --machine
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

### `--local`

Use `--local` on one LAN when you do not want rendezvous:

```sh
skyffla copper-731 --local
```

```sh
skyffla copper-731 --local
```

Or make one side the explicit host:

```sh
skyffla copper-731 --host --local
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
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 skyffla copper-731
```

`skyffla-rendezvous` ignores `X-Forwarded-For` by default. Only set
`SKYFFLA_RENDEZVOUS_TRUST_PROXY_HEADERS=true` when it is behind a trusted proxy
that you control.

## Docs

- [`docs/room-architecture.md`](docs/room-architecture.md): room-native architecture and design boundaries
- [`docs/machine-protocol.md`](docs/machine-protocol.md): wrapper-facing `machine` contract
- [`docs/iroh-infra-notes.md`](docs/iroh-infra-notes.md): self-hosting guidance for rendezvous, relays, and blob transfer infrastructure
- [`docs/versioning.md`](docs/versioning.md): compatibility rules for wire, machine, and rendezvous protocols
- [`docs/environment.md`](docs/environment.md): environment variables for the CLI, rendezvous server, wrappers, tests, and release tooling

## Local Development

With Rust installed:

```sh
cargo build --bins
```

```sh
cargo test -p skyffla
```

Run the Python wrapper checks:

```sh
cd wrappers/python
uv sync
uv run pytest
```

Run the Node.js wrapper checks:

```sh
npm ci --prefix wrappers/node
npm test --prefix wrappers/node
```

Run the rendezvous server locally:

```sh
cargo run -p skyffla-rendezvous
```

Join a room against that local server:

```sh
SKYFFLA_RENDEZVOUS_URL=http://127.0.0.1:8080 cargo run -p skyffla -- copper-731
```
