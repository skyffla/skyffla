# Skyffla Python Wrapper

This package is a thin async wrapper over `skyffla host|join <room> machine --json`.

## Install

```sh
cd wrappers/python
uv sync
```

The wrapper expects a `skyffla` binary in `PATH`. Override it with `SKYFFLA_BIN`
or the `binary=` argument when starting a room.

Versioning is split across two layers:

- machine protocol compatibility follows [`docs/versioning.md`](../../docs/versioning.md)
  and [`docs/machine-protocol.md`](../../docs/machine-protocol.md): the wrapper
  requires the same machine protocol major version and tolerates additive minor
  changes
- release pairing is stricter for published packages: by default, the wrapper
  also checks that the spawned `skyffla` binary has the exact same release
  version as the Python package

Set `SKYFFLA_SKIP_VERSION_CHECK=1` only when you intentionally want to bypass
the release-version pairing check. It does not bypass the machine protocol
compatibility check.

## Async Example

```python
import asyncio

from skyffla import Room


async def main() -> None:
    async with await Room.join("warehouse", name="python-agent") as room:
        await room.wait_for_welcome()
        await room.send_chat("all", "hello from python")
        print(await room.wait_for_chat())


asyncio.run(main())
```

## Sync Example

```python
from skyffla import SyncRoom


with SyncRoom.join("warehouse", name="python-agent") as room:
    room.wait_for_welcome()
    room.send_chat("all", "hello from sync python")
    print(room.wait_for_chat())
```

For a minimal runnable script, see [`examples/simple_chat.py`](examples/simple_chat.py).

Same machine or LAN:

```sh
uv run python examples/simple_chat.py host demo-room --local
uv run python examples/simple_chat.py join demo-room --local
```

Or let both sides use `join` and allow the first process to promote itself to
host:

```sh
uv run python examples/simple_chat.py join demo-room --local
uv run python examples/simple_chat.py join demo-room --local
```

`examples/simple_chat.py` is now a minimal interactive chat client. Start one
host and one join process, then type lines in either terminal. Use `/quit` to
exit.

For `--local`, start the host first and give local discovery a few seconds to
advertise before starting the join side. If the join side promotes itself to
host, use `--server` with a local `skyffla-rendezvous` for deterministic
same-machine testing.

Custom rendezvous server:

```sh
uv run python examples/simple_chat.py host demo-room --server http://127.0.0.1:8080
uv run python examples/simple_chat.py join demo-room --server http://127.0.0.1:8080
```

## Tests

```sh
cd wrappers/python
uv run pytest
```
