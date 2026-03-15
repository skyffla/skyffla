# Simple Chat

Minimal interactive chat client using the published `skyffla` Python wrapper.

Install `skyffla`, then from this directory run:

```sh
uv sync
```

Run two terminals:

```sh
uv run python simple-chat/main.py join demo-room --local
uv run python simple-chat/main.py join demo-room --local
```

If you want to use a local repo build of `skyffla` instead of the installed
binary, set `SKYFFLA_BIN=../../target/debug/skyffla`.
