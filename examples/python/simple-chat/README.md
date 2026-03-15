# Simple Chat

Minimal interactive chat client using the published `skyffla` Python wrapper.

From the repo root:

```sh
cargo build --bins
uv sync --project examples/python
```

Run two terminals:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python python examples/python/simple-chat/main.py join demo-room --local
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python python examples/python/simple-chat/main.py join demo-room --local
```
