# Skyffla Python Wrapper

Use this package to control an installed `skyffla` CLI from Python.

`pip install skyffla` installs the Python wrapper only. Install the `skyffla`
binary separately first.

## 1. Install the `skyffla` binary

Add the Homebrew tap once:

```sh
brew tap skyffla/skyffla
```

Install the CLI:

```sh
brew install skyffla
```

Check that the binary is available:

```sh
skyffla --version
```

If you want source code, release notes, or other install paths, see the
[Skyffla repository on GitHub](https://github.com/skyffla/skyffla).

## 2. Install the Python wrapper

```sh
python -m pip install skyffla
```

The wrapper expects a `skyffla` binary in `PATH`. Override it with `SKYFFLA_BIN`
or the `binary=` argument when starting a room.

## 3. Small example

Save this as `example.py`. Start `host` in one terminal, then start `join` in
another.

```python
import asyncio
import sys

from skyffla import Room


async def main(role: str) -> None:
    opener = Room.host if role == "host" else Room.join

    async with await opener("warehouse", name=role) as room:
        await room.wait_for_welcome()
        if role == "host":
            await room.wait_for_member_joined(name="join")
            await room.send_chat("all", "hello from python")
            print(await room.wait_for_chat(from_name="join"))
        else:
            await room.wait_for_member_snapshot(min_members=2)
            print(await room.wait_for_chat(from_name="host"))
            await room.send_chat("all", "hello from join")


asyncio.run(main(sys.argv[1]))
```

```sh
python example.py host
python example.py join
```

## Version checks

Versioning is split across two layers:

- machine protocol compatibility follows the
  [versioning policy](https://github.com/skyffla/skyffla/blob/main/docs/versioning.md)
  and
  [machine protocol reference](https://github.com/skyffla/skyffla/blob/main/docs/machine-protocol.md):
  the wrapper requires the same machine protocol major version and tolerates
  additive minor changes
- release pairing is stricter for published packages: by default, the wrapper
  also checks that the spawned `skyffla` binary has the exact same release
  version as the Python package

Set `SKYFFLA_SKIP_VERSION_CHECK=1` only if you intentionally want to run the
wrapper against a different `skyffla` release. This skips the package-to-binary
release match, but machine protocol compatibility is still enforced.

## More examples

Runnable example apps live under `examples/python` in the GitHub repository:

- [examples/python](https://github.com/skyffla/skyffla/tree/main/examples/python):
  shared Python example project
- [examples/python/room-agents-studio](https://github.com/skyffla/skyffla/tree/main/examples/python/room-agents-studio):
  multi-agent OpenAI poster studio with a live browser gallery and image
  transfer

## Tests

From a repository checkout:

```sh
cd wrappers/python
uv run pytest
```
