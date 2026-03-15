# Python Room Agents Demo

This demo shows how to use the Skyffla Python wrapper from Python code with
three model-driven agents in separate terminal windows:

- one director
- multiple artists

The demo runs a counting game:

- the director assigns numbers `1, 2, 3...`
- artists take turns in round-robin order
- each artist draws their assigned number as terminal-friendly ASCII art
- the director waits for each reply before giving the next number to the next artist

The default OpenAI model is `gpt-4o-mini`.

- the director uses it to draft concise turn instructions
- each artist uses it to choose a visual style preset
- the actual numeral is rendered by a deterministic local ASCII digit engine so
  the output stays readable in the terminal

## Setup

From this directory:

```sh
cp .env.example .env
```

Edit `.env` and set at least:

```sh
OPENAI_API_KEY=...
```

Then sync the demo environment:

```sh
uv sync
```

Build the local Skyffla CLI once from the repo root:

```sh
cargo build --bins
```

## Run With Three Terminals

Terminal 1, start the director:

```sh
cd demos/python-room-agents
SKYFFLA_BIN=../../target/debug/skyffla uv run room-director demo-room --local --name director
```

Terminal 2, start the first artist:

```sh
cd demos/python-room-agents
SKYFFLA_BIN=../../target/debug/skyffla uv run room-artist demo-room --local --name artist-outline
```

Terminal 3, start the second artist:

```sh
cd demos/python-room-agents
SKYFFLA_BIN=../../target/debug/skyffla uv run room-artist demo-room --local --name artist-shading
```

If local discovery is flaky on one machine, replace `--local` with a rendezvous
server:

```sh
SKYFFLA_BIN=../../target/debug/skyffla uv run room-director demo-room --server http://127.0.0.1:8080 --name director
SKYFFLA_BIN=../../target/debug/skyffla uv run room-artist demo-room --server http://127.0.0.1:8080 --name artist-outline
```

## Configuration

The demo loads environment variables from `.env` in this directory.

- `OPENAI_API_KEY`: required
- `OPENAI_MODEL`: default model for both agents, defaults to `gpt-4o-mini`
- `DIRECTOR_MODEL`: optional override for the director
- `ARTIST_MODEL`: optional override for artists
- `COUNT_GRID_WIDTH`: width of the ASCII art grid, defaults to `24`
- `COUNT_GRID_HEIGHT`: height of the ASCII art grid, defaults to `11`
- `COUNTING_THEME`: optional style/theme hint, defaults to `playful marquee numerals`

## Notes

- The director always hosts the room in this first version.
- Artists always join the room and reply directly to the host member.
- The current protocol uses direct room chat messages so it stays easy to read
  and inspect while we refine the behavior.
- Artists clear the terminal on each turn, print a short receipt line, and then
  show only the number they painted.
- The director keeps a concise send/receive log and does not reprint returned
  ASCII art.
- The default model constant lives in `src/room_agents_demo/shared.py` as
  `DEFAULT_MODEL`, and can be overridden with `OPENAI_MODEL`,
  `DIRECTOR_MODEL`, or `ARTIST_MODEL`.
- The wrapper is imported directly from the repo checkout under
  `wrappers/python/src`, so this demo stays self-contained inside the repo.
