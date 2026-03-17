# Skyffla Python Examples

These examples are intentionally separate from the wrapper source tree. They
install the published [`skyffla`](https://pypi.org/project/skyffla/) package
from PyPI so the setup stays close to what an external user would do.
The checked-in dependency version should track the latest published wrapper release.

## Setup

Install `skyffla` first, for example with Homebrew:

```sh
brew install skyffla
```

Then change into this directory:

```sh
cd examples/python
uv sync
```

If you want to run against a local repo build instead of the installed binary,
set `SKYFFLA_BIN`, for example `SKYFFLA_BIN=../../target/debug/skyffla`.

## Included Examples

- [`simple-chat`](simple-chat): minimal interactive chat client for one room
- [`sync-chat-and-channel`](sync-chat-and-channel): sync chat plus one machine channel exchange
- [`room-agents-studio`](room-agents-studio): multi-agent OpenAI poster studio with a live gallery

## Run

Simple chat:

```sh
uv run python simple-chat/main.py join demo-room --local
uv run python simple-chat/main.py join demo-room --local
```

Chat plus machine channel:

```sh
uv run python sync-chat-and-channel/main.py host demo-room
uv run python sync-chat-and-channel/main.py join demo-room
```

Poster studio:

See [`room-agents-studio/README.md`](room-agents-studio/README.md).
