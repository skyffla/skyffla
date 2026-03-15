# Sync Chat And Channel

Synchronous example that sends one room chat message and one machine-channel
message.

From the repo root:

```sh
cargo build --bins
uv sync --project examples/python
```

Run two terminals:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python python examples/python/sync-chat-and-channel/main.py host demo-room
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python python examples/python/sync-chat-and-channel/main.py join demo-room
```

Or use the installed entry point:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python sync-chat-and-channel host demo-room
```
