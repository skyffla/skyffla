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

## Examples

Runnable example apps live under [`../../examples/python`](../../examples/python).
That project deliberately installs the published `skyffla` package from PyPI so
the setup matches external usage rather than importing this source tree
directly.

Included examples:

- [`../../examples/python`](../../examples/python): shared Python example project
- [`../../examples/python/room-agents-studio/README.md`](../../examples/python/room-agents-studio/README.md): multi-agent OpenAI poster studio with a live browser gallery and image transfer

## Tests

```sh
cd wrappers/python
uv run pytest
```
