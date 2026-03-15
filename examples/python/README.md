# Skyffla Python Examples

These examples are intentionally separate from the wrapper source tree. They
install the published [`skyffla`](https://pypi.org/project/skyffla/) package
from PyPI so the setup stays close to what an external user would do.

## Setup

From the repo root:

```sh
cargo build --bins
uv sync --project examples/python
```

The examples use the published Python wrapper, but they still need a matching
`skyffla` CLI binary. When running from the repo checkout, point them at the
locally built binary:

```sh
SKYFFLA_BIN=target/debug/skyffla
```

## Included Examples

- `simple-chat`: minimal interactive chat client for one room
- `sync-chat-and-channel`: sync chat plus one machine channel exchange
- `room-director` / `room-artist`: multi-agent OpenAI poster studio with a live gallery

## Run

Simple chat:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python simple-chat join demo-room --local
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python simple-chat join demo-room --local
```

Chat plus machine channel:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python sync-chat-and-channel host demo-room
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python sync-chat-and-channel join demo-room
```

Poster studio:

See [`room-agents-studio/README.md`](room-agents-studio/README.md).
