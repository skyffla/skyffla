# Sync Chat And Channel

Synchronous example that sends one room chat message and one machine-channel
message.

Install `skyffla`, then from this directory run:

```sh
uv sync
```

Run two terminals:

```sh
uv run python sync-chat-and-channel/main.py host demo-room
uv run python sync-chat-and-channel/main.py join demo-room
```

If you want to use a local repo build of `skyffla` instead of the installed
binary, set `SKYFFLA_BIN=../../target/debug/skyffla`.
