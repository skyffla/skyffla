from __future__ import annotations

import os
import sys
import textwrap
import uuid
from dataclasses import dataclass
from pathlib import Path

from dotenv import load_dotenv
from openai import OpenAI


DEMO_DIR = Path(__file__).resolve().parents[2]
REPO_ROOT = DEMO_DIR.parents[1]
WRAPPER_SRC = REPO_ROOT / "wrappers" / "python" / "src"

if str(WRAPPER_SRC) not in sys.path:
    sys.path.insert(0, str(WRAPPER_SRC))

from skyffla import Member, SyncRoom  # noqa: E402


DEFAULT_MODEL = "gpt-4o-mini"
DEFAULT_GRID_WIDTH = 24
DEFAULT_GRID_HEIGHT = 11
DEFAULT_COUNTING_THEME = "playful marquee numerals"
STYLE_HINTS = (
    "bold blocky scoreboard",
    "neon sign glow",
    "chunky arcade cabinet",
    "ornamental poster type",
)
STYLE_PRESETS: dict[str, dict[str, str]] = {
    "scoreboard": {"on": "#", "shadow": ".", "accent": "="},
    "neon": {"on": "=", "shadow": ".", "accent": "~"},
    "arcade": {"on": "@", "shadow": ":", "accent": "+"},
    "poster": {"on": "*", "shadow": ".", "accent": "-"},
}
DEFAULT_STYLE_KEY = "scoreboard"
DIGIT_GLYPHS = {
    "0": (" ### ", "#   #", "#  ##", "# # #", "##  #", "#   #", " ### "),
    "1": ("  #  ", " ##  ", "# #  ", "  #  ", "  #  ", "  #  ", "#####"),
    "2": (" ### ", "#   #", "    #", " ### ", "#    ", "#    ", "#####"),
    "3": (" ### ", "#   #", "    #", " ### ", "    #", "#   #", " ### "),
    "4": ("#   #", "#   #", "#   #", "#####", "    #", "    #", "    #"),
    "5": ("#####", "#    ", "#    ", "#### ", "    #", "#   #", " ### "),
    "6": (" ### ", "#   #", "#    ", "#### ", "#   #", "#   #", " ### "),
    "7": ("#####", "    #", "   # ", "  #  ", " #   ", " #   ", " #   "),
    "8": (" ### ", "#   #", "#   #", " ### ", "#   #", "#   #", " ### "),
    "9": (" ### ", "#   #", "#   #", " ####", "    #", "#   #", " ### "),
}


@dataclass(frozen=True)
class TaskMessage:
    task_id: str
    number: int
    width: int
    height: int
    style_hint: str
    brief: str


@dataclass(frozen=True)
class ArtMessage:
    task_id: str
    number: int
    artist_name: str
    art: str


@dataclass(frozen=True)
class DemoConfig:
    api_key: str
    director_model: str
    artist_model: str
    grid_width: int
    grid_height: int
    counting_theme: str


def load_demo_config() -> DemoConfig:
    load_dotenv(DEMO_DIR / ".env", override=False)

    api_key = os.environ.get("OPENAI_API_KEY", "").strip()
    if not api_key:
        raise SystemExit("OPENAI_API_KEY is required in demos/python-room-agents/.env")

    default_model = os.environ.get("OPENAI_MODEL", DEFAULT_MODEL).strip() or DEFAULT_MODEL
    director_model = os.environ.get("DIRECTOR_MODEL", default_model).strip() or default_model
    artist_model = os.environ.get("ARTIST_MODEL", default_model).strip() or default_model
    grid_width = _parse_positive_int(
        os.environ.get("COUNT_GRID_WIDTH"),
        default=DEFAULT_GRID_WIDTH,
        name="COUNT_GRID_WIDTH",
    )
    grid_height = _parse_positive_int(
        os.environ.get("COUNT_GRID_HEIGHT"),
        default=DEFAULT_GRID_HEIGHT,
        name="COUNT_GRID_HEIGHT",
    )
    counting_theme = (
        os.environ.get("COUNTING_THEME", DEFAULT_COUNTING_THEME).strip() or DEFAULT_COUNTING_THEME
    )

    return DemoConfig(
        api_key=api_key,
        director_model=director_model,
        artist_model=artist_model,
        grid_width=grid_width,
        grid_height=grid_height,
        counting_theme=counting_theme,
    )


def _parse_positive_int(raw: str | None, *, default: int, name: str) -> int:
    if raw is None or not raw.strip():
        return default
    try:
        value = int(raw)
    except ValueError as exc:
        raise SystemExit(f"{name} must be an integer") from exc
    if value <= 0:
        raise SystemExit(f"{name} must be positive")
    return value


def build_openai_client(api_key: str) -> OpenAI:
    return OpenAI(api_key=api_key)


def choose_style_hint(index: int) -> str:
    return STYLE_HINTS[index % len(STYLE_HINTS)]


def choose_style_key_from_hint(style_hint: str) -> str:
    lowered = style_hint.lower()
    if "neon" in lowered:
        return "neon"
    if "arcade" in lowered:
        return "arcade"
    if "poster" in lowered:
        return "poster"
    return DEFAULT_STYLE_KEY


def make_task_message(*, number: int, width: int, height: int, style_hint: str, brief: str) -> TaskMessage:
    return TaskMessage(
        task_id=uuid.uuid4().hex[:8],
        number=number,
        width=width,
        height=height,
        style_hint=style_hint,
        brief=brief.strip(),
    )


def render_task_message(task: TaskMessage) -> str:
    return (
        f"TASK {task.task_id}\n"
        f"number: {task.number}\n"
        f"grid: {task.width}x{task.height}\n"
        f"style: {task.style_hint}\n"
        "brief:\n"
        f"{task.brief}"
    )


def parse_task_message(text: str) -> TaskMessage | None:
    lines = text.splitlines()
    if len(lines) < 4 or not lines[0].startswith("TASK "):
        return None
    if not lines[1].startswith("number: "):
        return None
    if not lines[2].startswith("grid: "):
        return None
    if not lines[3].startswith("style: "):
        return None
    if lines[4] != "brief:":
        return None

    brief = "\n".join(lines[5:]).strip()
    if not brief:
        return None

    grid_value = lines[2][len("grid: ") :].strip()
    if "x" not in grid_value:
        return None
    width_text, height_text = grid_value.split("x", 1)

    return TaskMessage(
        task_id=lines[0].split(" ", 1)[1].strip(),
        number=int(lines[1][len("number: ") :].strip()),
        width=int(width_text),
        height=int(height_text),
        style_hint=lines[3][len("style: ") :].strip(),
        brief=brief,
    )


def render_art_message(*, task_id: str, number: int, artist_name: str, art: str) -> str:
    return (
        f"ART {task_id}\n"
        f"number: {number}\n"
        f"artist: {artist_name}\n"
        "art:\n"
        f"{normalize_ascii_art(art)}"
    )


def parse_art_message(text: str) -> ArtMessage | None:
    lines = text.splitlines()
    if len(lines) < 4 or not lines[0].startswith("ART "):
        return None
    if not lines[1].startswith("number: "):
        return None
    if not lines[2].startswith("artist: "):
        return None
    if lines[3] != "art:":
        return None

    art = "\n".join(lines[4:]).rstrip()
    if not art:
        return None

    return ArtMessage(
        task_id=lines[0].split(" ", 1)[1].strip(),
        number=int(lines[1][len("number: ") :].strip()),
        artist_name=lines[2][len("artist: ") :].strip(),
        art=art,
    )


def normalize_ascii_art(text: str, *, max_width: int = 36, max_lines: int = 16) -> str:
    cleaned_lines = [line.rstrip() for line in text.strip("\n").splitlines() if line.strip()]
    if not cleaned_lines:
        return "[empty art]"
    limited = cleaned_lines[:max_lines]
    return "\n".join(line[:max_width] for line in limited)


def fallback_brief(number: int, style_hint: str, width: int, height: int, counting_theme: str) -> str:
    return (
        f"Draw the number {number} as {style_hint} ASCII art with a {counting_theme} feel. "
        f"Fit it within a {width}x{height} terminal grid and make the numeral obvious at a glance."
    )


def draft_director_brief(
    *,
    client: OpenAI,
    model: str,
    number: int,
    width: int,
    height: int,
    counting_theme: str,
    style_hint: str,
    artist_name: str,
) -> str:
    response = client.responses.create(
        model=model,
        instructions=(
            "You are a creative director briefing an ASCII artist for a counting game. "
            "Write one short instruction block, no bullets, no markdown, under 80 words."
        ),
        input=(
            f"Artist name: {artist_name}\n"
            f"Number to draw: {number}\n"
            f"Grid size: {width}x{height}\n"
            f"Theme: {counting_theme}\n"
            f"Preferred style: {style_hint}\n"
            "Mention that the numeral must be legible immediately in a terminal window."
        ),
    )
    brief = response.output_text.strip()
    return brief or fallback_brief(number, style_hint, width, height, counting_theme)


def generate_ascii_art(
    *,
    client: OpenAI,
    model: str,
    artist_name: str,
    task: TaskMessage,
) -> str:
    style_key = choose_render_style(
        client=client,
        model=model,
        artist_name=artist_name,
        task=task,
    )
    return render_number_art(
        number=task.number,
        width=task.width,
        height=task.height,
        style_key=style_key,
    )


def choose_render_style(
    *,
    client: OpenAI,
    model: str,
    artist_name: str,
    task: TaskMessage,
) -> str:
    response = client.responses.create(
        model=model,
        instructions=(
            "You are choosing one rendering preset for a deterministic ASCII number renderer. "
            "Reply with exactly one word from this list: scoreboard, neon, arcade, poster."
        ),
        input=(
            f"Artist name: {artist_name}\n"
            f"Number: {task.number}\n"
            f"Grid: {task.width}x{task.height}\n"
            f"Style: {task.style_hint}\n"
            f"Brief: {task.brief}\n"
            "Pick the one preset that best matches the brief while keeping the number easy to read."
        ),
    )
    style_key = response.output_text.strip().splitlines()[0].strip().lower()
    if style_key not in STYLE_PRESETS:
        return choose_style_key_from_hint(task.style_hint)
    return style_key


def format_member(member: Member) -> str:
    return f"{member.name} ({member.member_id})"


def print_room_ready(role: str, room: SyncRoom, *, self_member: str, host_member: str) -> None:
    print(
        f"[{role}] room={room.room_id} self={self_member} host={host_member} argv={' '.join(room.argv)}"
    )


def clear_screen() -> None:
    print("\033[2J\033[H", end="", flush=True)


def wrap_block(title: str, body: str) -> str:
    return f"{title}\n{textwrap.indent(body.rstrip(), prefix='    ')}"


def choose_next_artist(artist_ids: list[str], last_artist_id: str | None) -> str | None:
    if not artist_ids:
        return None
    if last_artist_id is None or last_artist_id not in artist_ids:
        return artist_ids[0]
    index = artist_ids.index(last_artist_id)
    return artist_ids[(index + 1) % len(artist_ids)]


def frame_ascii_art(title: str, art: str, *, width: int) -> str:
    lines = [line.rstrip() for line in art.splitlines()]
    inner_width = max(width, len(title), *(len(line) for line in lines))
    top = f"+- {title.ljust(inner_width)} -+"
    body = [f"| {line.ljust(inner_width)} |" for line in lines]
    bottom = f"+-{'-' * inner_width}-+"
    return "\n".join([top, *body, bottom])


def render_number_art(*, number: int, width: int, height: int, style_key: str) -> str:
    preset = STYLE_PRESETS.get(style_key, STYLE_PRESETS[DEFAULT_STYLE_KEY])
    glyph_rows = _compose_digit_rows(str(number))
    canvas = [[" " for _ in range(width)] for _ in range(height)]

    glyph_height = len(glyph_rows)
    glyph_width = max(len(row) for row in glyph_rows)
    x0 = max((width - glyph_width) // 2, 0)
    y0 = max((height - glyph_height) // 2, 0)

    for gy, row in enumerate(glyph_rows):
        for gx, char in enumerate(row):
            if char == " ":
                continue
            x = x0 + gx
            y = y0 + gy
            if 0 <= x < width and 0 <= y < height:
                canvas[y][x] = preset["on"]
            sx = x + 1
            sy = y + 1
            if 0 <= sx < width and 0 <= sy < height and canvas[sy][sx] == " ":
                canvas[sy][sx] = preset["shadow"]

    accent_char = preset["accent"]
    for x in range(max(0, x0 - 1), min(width, x0 + glyph_width + 1)):
        if y0 > 0 and canvas[y0 - 1][x] == " ":
            canvas[y0 - 1][x] = accent_char

    lines = ["".join(row).rstrip() for row in canvas]
    trimmed = _trim_vertical(lines)
    return "\n".join(trimmed)


def _compose_digit_rows(number_text: str) -> list[str]:
    rows = [""] * 7
    for index, digit in enumerate(number_text):
        glyph = DIGIT_GLYPHS.get(digit, DIGIT_GLYPHS["0"])
        for row_index, glyph_row in enumerate(glyph):
            if index:
                rows[row_index] += " "
            rows[row_index] += glyph_row
    return rows


def _trim_vertical(lines: list[str]) -> list[str]:
    kept = [line for line in lines if line.strip()]
    return kept if kept else ["[empty art]"]
