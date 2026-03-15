# Room Agents Studio

This example shows how to use the published Skyffla Python wrapper from Python
code with three model-driven agents in separate terminal windows:

- one director
- multiple artists

The current version is a distributed image studio:

- the director waits for a topic in the terminal
- artists can join at any time
- all artists connected at submit time enter that round
- each artist gets a different creative persona and direction
- artists generate a real image locally with OpenAI image generation
- artists send submission metadata over chat and PNGs over Skyffla file transfer
- the director serves a live localhost gallery that updates as metadata and
  images arrive
- once all current-round images arrive, the director picks a winner

## Models

The demo uses:

- `gpt-5.4` by default for:
  - artist-direction drafting
  - artist concept/prompt planning
  - winner selection
- `gpt-image-1.5` by default for the final image generation

The current OpenAI Python SDK in this demo environment exposes image generation
through `client.images.generate(...)`, and GPT image models return base64 image
data that the artists save to `png` before sending.

## Setup

From the repo root:

```sh
cargo build --bins
uv sync --project examples/python
```

Then create a local `.env` for this example:

```sh
cp examples/python/room-agents-studio/.env.example examples/python/room-agents-studio/.env
```

Edit `examples/python/room-agents-studio/.env` and set at least:

```sh
OPENAI_API_KEY=...
```

## Run With Three Terminals

Terminal 1, start the director:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python room-director image-room --name director
```

Terminal 2, start the first artist:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python room-artist image-room --name artist-outline
```

Terminal 3, start the second artist:

```sh
SKYFFLA_BIN=target/debug/skyffla uv run --project examples/python room-artist image-room --name artist-shading
```

These commands use the normal rendezvous-backed room flow, so the example works
across networks rather than depending on local discovery.

The director prints the gallery URL at startup. By default it is:

```text
http://127.0.0.1:8765
```

Open that in a browser before you start a round. The page polls once per
second and shows:

- connected artists
- current topic
- submission status per artist
- title/tagline/concept as soon as metadata arrives
- images as soon as file transfer completes
- winner and jury reason once judging finishes

## Suggested Topics

These tend to work well because they imply a strong focal scene:

- `forbidden aquarium grand opening`
- `ghost orchestra in a flooded subway`
- `neon lighthouse for lost robots`
- `witch bakery in a thunderstorm`
- `moth festival at the observatory`
- `cathedral built inside a whale skeleton`
- `volcanic chess tournament`
- `interstellar flea market at dawn`

## Configuration

The example loads environment variables from
`examples/python/room-agents-studio/.env`, but a globally exported
`OPENAI_API_KEY` also works.

- `OPENAI_API_KEY`: required
- `OPENAI_MODEL`: default text model, defaults to `gpt-5.4`
- `DIRECTOR_MODEL`: optional override for artist-direction drafting
- `ARTIST_MODEL`: optional override for artist concept/prompt planning
- `JUDGE_MODEL`: optional override for winner selection
- `IMAGE_MODEL`: image model, defaults to `gpt-image-1.5`
- `IMAGE_SIZE`: defaults to `1536x1024`
- `IMAGE_QUALITY`: defaults to `medium`
- `DIRECTOR_TEMPERATURE`: defaults to `0.9`, used on non-GPT-5 text models
- `ARTIST_TEMPERATURE`: defaults to `1.1`, used on non-GPT-5 text models
- `JUDGE_TEMPERATURE`: defaults to `0.8`, used on non-GPT-5 text models
- `POSTER_THEME`: shared creative flavor for director briefs
- `GALLERY_HOST`: defaults to `127.0.0.1`
- `GALLERY_PORT`: defaults to `8765`
- `DEMO_OUTPUT_DIR`: defaults to `examples/python/room-agents-studio/output`

## Output

The demo writes files under this directory by default:

- `output/rounds/<round-id>/...` for exported images on the director side
- `output/artist-cache/<artist>/<round-id>/...` for each artist’s local generated file

## Notes

- The browser is the main shared view now; terminals are mostly for agent logs.
- The first implementation uses simple polling rather than SSE or websockets.
- The example installs `skyffla` from PyPI through
  `examples/python/pyproject.toml`.
- The binary and package versions must still match. If you run against the repo
  build, use the current repo release version.
