from __future__ import annotations

import base64
import os
import random
import uuid
from pathlib import Path
from typing import Any, TypeVar

from dotenv import load_dotenv
from openai import OpenAI
from pydantic import BaseModel, ConfigDict, ValidationError, field_validator
from skyffla import Member, SyncRoom


EXAMPLES_ROOT = Path(__file__).resolve().parents[3]
STUDIO_DIR = EXAMPLES_ROOT / "room-agents-studio"


DEFAULT_MODEL = "gpt-5.4"
DEFAULT_IMAGE_MODEL = "gpt-image-1.5"
DEFAULT_IMAGE_SIZE = "1536x1024"
DEFAULT_IMAGE_QUALITY = "medium"
DEFAULT_POSTER_THEME = "retro-futurist secret-event campaign"
DEFAULT_DIRECTOR_TEMPERATURE = 0.9
DEFAULT_ARTIST_TEMPERATURE = 1.1
DEFAULT_JUDGE_TEMPERATURE = 0.8
DEFAULT_GALLERY_HOST = "127.0.0.1"
DEFAULT_GALLERY_PORT = 8765
ARTIST_PERSONAS: tuple[tuple[str, str], ...] = (
    (
        "hero-silhouette",
        "One giant hero subject, dramatic negative space, and a shape that reads instantly.",
    ),
    (
        "noir-atmosphere",
        "Moody cinematic lighting, haze, reflections, and selective detail around one clear focal scene.",
    ),
    (
        "typographic-spectacle",
        "The title and image should lock together like a real campaign poster, not separate elements.",
    ),
    (
        "collage-surreal",
        "2-3 surreal motifs, diagonal energy, and a playful but intentional composition.",
    ),
    (
        "ornate-propaganda",
        "Ceremonial grandeur, symbolic emblems, and a suspiciously persuasive sense of occasion.",
    ),
    (
        "luxury-editorial",
        "High-fashion framing, expensive materials, and an immaculate sense of staged glamour.",
    ),
    (
        "pulp-adventure",
        "Bold peril, exaggerated scale, and a kinetic cover-illustration energy.",
    ),
    (
        "dreamy-romance",
        "Soft glow, elegant yearning, and a poster that feels emotionally oversized in the best way.",
    ),
    (
        "technical-cutaway",
        "Blueprint clarity, labeled intrigue, and a polished systems-diagram fascination around the main spectacle.",
    ),
    (
        "festival-psychedelia",
        "Color-burst exuberance, rhythmic repetition, and a joyful sense of visual overload with one clear focal hook.",
    ),
)


class DemoModel(BaseModel):
    model_config = ConfigDict(frozen=True, str_strip_whitespace=True)


class StudioTask(DemoModel):
    task_id: str
    round_id: str
    topic: str
    persona_name: str
    direction: str


class SubmissionMetadata(DemoModel):
    task_id: str
    round_id: str
    artist_name: str
    title: str
    tagline: str
    concept: str
    prompt_summary: str
    image_channel_id: str
    image_name: str

    @field_validator("title", "tagline", "concept", "prompt_summary")
    @classmethod
    def _sanitize_inline_fields(cls, value: str) -> str:
        return sanitize_inline(value)

    @field_validator("image_name")
    @classmethod
    def _sanitize_image_name(cls, value: str) -> str:
        return sanitize_filename(value)


class WinnerDecision(DemoModel):
    winner_name: str
    reason: str

    @field_validator("winner_name", "reason")
    @classmethod
    def _sanitize_fields(cls, value: str) -> str:
        return sanitize_inline(value)


class ImagePlan(DemoModel):
    title: str
    tagline: str
    concept: str
    prompt_summary: str
    image_prompt: str

    @field_validator("title", "tagline", "concept", "prompt_summary")
    @classmethod
    def _sanitize_inline_fields(cls, value: str) -> str:
        return sanitize_inline(value)


class DemoConfig(DemoModel):
    api_key: str
    director_model: str
    artist_model: str
    judge_model: str
    image_model: str
    image_size: str
    image_quality: str
    director_temperature: float
    artist_temperature: float
    judge_temperature: float
    poster_theme: str
    gallery_host: str
    gallery_port: int
    output_dir: Path


def load_demo_config() -> DemoConfig:
    load_dotenv(STUDIO_DIR / ".env", override=False)

    api_key = os.environ.get("OPENAI_API_KEY", "").strip()
    if not api_key:
        raise SystemExit("OPENAI_API_KEY is required in examples/python/room-agents-studio/.env or the environment")

    default_model = os.environ.get("OPENAI_MODEL", DEFAULT_MODEL).strip() or DEFAULT_MODEL
    director_model = os.environ.get("DIRECTOR_MODEL", default_model).strip() or default_model
    artist_model = os.environ.get("ARTIST_MODEL", default_model).strip() or default_model
    judge_model = os.environ.get("JUDGE_MODEL", director_model).strip() or director_model
    image_model = os.environ.get("IMAGE_MODEL", DEFAULT_IMAGE_MODEL).strip() or DEFAULT_IMAGE_MODEL
    image_size = os.environ.get("IMAGE_SIZE", DEFAULT_IMAGE_SIZE).strip() or DEFAULT_IMAGE_SIZE
    image_quality = os.environ.get("IMAGE_QUALITY", DEFAULT_IMAGE_QUALITY).strip() or DEFAULT_IMAGE_QUALITY
    director_temperature = _parse_float_in_range(
        os.environ.get("DIRECTOR_TEMPERATURE"),
        default=DEFAULT_DIRECTOR_TEMPERATURE,
        name="DIRECTOR_TEMPERATURE",
    )
    artist_temperature = _parse_float_in_range(
        os.environ.get("ARTIST_TEMPERATURE"),
        default=DEFAULT_ARTIST_TEMPERATURE,
        name="ARTIST_TEMPERATURE",
    )
    judge_temperature = _parse_float_in_range(
        os.environ.get("JUDGE_TEMPERATURE"),
        default=DEFAULT_JUDGE_TEMPERATURE,
        name="JUDGE_TEMPERATURE",
    )
    poster_theme = os.environ.get("POSTER_THEME", DEFAULT_POSTER_THEME).strip() or DEFAULT_POSTER_THEME
    gallery_host = os.environ.get("GALLERY_HOST", DEFAULT_GALLERY_HOST).strip() or DEFAULT_GALLERY_HOST
    gallery_port = _parse_port(os.environ.get("GALLERY_PORT"), default=DEFAULT_GALLERY_PORT)
    output_dir = Path(os.environ.get("DEMO_OUTPUT_DIR", "output")).expanduser()
    if not output_dir.is_absolute():
        output_dir = STUDIO_DIR / output_dir

    return DemoConfig(
        api_key=api_key,
        director_model=director_model,
        artist_model=artist_model,
        judge_model=judge_model,
        image_model=image_model,
        image_size=image_size,
        image_quality=image_quality,
        director_temperature=director_temperature,
        artist_temperature=artist_temperature,
        judge_temperature=judge_temperature,
        poster_theme=poster_theme,
        gallery_host=gallery_host,
        gallery_port=gallery_port,
        output_dir=output_dir,
    )


def _parse_port(raw: str | None, *, default: int) -> int:
    if raw is None or not raw.strip():
        return default
    try:
        value = int(raw)
    except ValueError as exc:
        raise SystemExit("GALLERY_PORT must be an integer") from exc
    if not 1 <= value <= 65535:
        raise SystemExit("GALLERY_PORT must be between 1 and 65535")
    return value


def _parse_float_in_range(raw: str | None, *, default: float, name: str) -> float:
    if raw is None or not raw.strip():
        return default
    try:
        value = float(raw)
    except ValueError as exc:
        raise SystemExit(f"{name} must be a number") from exc
    if not 0 <= value <= 2:
        raise SystemExit(f"{name} must be between 0 and 2")
    return value


def build_openai_client(api_key: str) -> OpenAI:
    return OpenAI(api_key=api_key)


def supports_temperature(model: str) -> bool:
    return not model.startswith("gpt-5")


def build_responses_kwargs(*, model: str, temperature: float, max_output_tokens: int) -> dict[str, Any]:
    kwargs: dict[str, Any] = {
        "model": model,
        "reasoning": {"effort": "low"},
        "text": {"verbosity": "low"},
        "max_output_tokens": max_output_tokens,
    }
    if supports_temperature(model):
        kwargs["temperature"] = temperature
    return kwargs


def new_round_id() -> str:
    return uuid.uuid4().hex[:6]


def new_channel_id(prefix: str = "img") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def choose_artist_personas(
    count: int,
    *,
    rng: random.Random | random.SystemRandom | None = None,
) -> list[tuple[str, str]]:
    if count <= 0:
        return []

    picker = rng or random.SystemRandom()
    pool = list(ARTIST_PERSONAS)
    chosen: list[tuple[str, str]] = []

    while len(chosen) < count:
        cycle = pool[:]
        picker.shuffle(cycle)
        chosen.extend(cycle)

    return chosen[:count]


def make_task_message(*, round_id: str, topic: str, persona_name: str, direction: str) -> StudioTask:
    return StudioTask(
        task_id=uuid.uuid4().hex[:8],
        round_id=round_id,
        topic=topic.strip(),
        persona_name=persona_name.strip(),
        direction=direction.strip(),
    )


def render_task_message(task: StudioTask) -> str:
    return (
        f"TASK {task.task_id}\n"
        f"round: {task.round_id}\n"
        f"topic: {task.topic}\n"
        f"persona: {task.persona_name}\n"
        "direction:\n"
        f"{task.direction}"
    )


def parse_task_message(text: str) -> StudioTask | None:
    lines = text.splitlines()
    if len(lines) < 5 or not lines[0].startswith("TASK "):
        return None
    if not lines[1].startswith("round: "):
        return None
    if not lines[2].startswith("topic: "):
        return None
    if not lines[3].startswith("persona: "):
        return None
    if lines[4] != "direction:":
        return None
    direction = "\n".join(lines[5:]).strip()
    if not direction:
        return None
    return _validate_demo_model(
        StudioTask,
        {
            "task_id": lines[0].split(" ", 1)[1].strip(),
            "round_id": lines[1][len("round: ") :].strip(),
            "topic": lines[2][len("topic: ") :].strip(),
            "persona_name": lines[3][len("persona: ") :].strip(),
            "direction": direction,
        },
    )


def render_submission_message(metadata: SubmissionMetadata) -> str:
    return (
        f"SUBMISSION {metadata.task_id}\n"
        f"round: {metadata.round_id}\n"
        f"artist: {metadata.artist_name}\n"
        f"title: {sanitize_inline(metadata.title)}\n"
        f"tagline: {sanitize_inline(metadata.tagline)}\n"
        f"concept: {sanitize_inline(metadata.concept)}\n"
        f"prompt_summary: {sanitize_inline(metadata.prompt_summary)}\n"
        f"image_channel: {metadata.image_channel_id}\n"
        f"image_name: {sanitize_filename(metadata.image_name)}"
    )


def parse_submission_message(text: str) -> SubmissionMetadata | None:
    lines = text.splitlines()
    if len(lines) != 9 or not lines[0].startswith("SUBMISSION "):
        return None
    prefixes = (
        "round: ",
        "artist: ",
        "title: ",
        "tagline: ",
        "concept: ",
        "prompt_summary: ",
        "image_channel: ",
        "image_name: ",
    )
    if not all(lines[index + 1].startswith(prefix) for index, prefix in enumerate(prefixes)):
        return None
    return _validate_demo_model(
        SubmissionMetadata,
        {
            "task_id": lines[0].split(" ", 1)[1].strip(),
            "round_id": lines[1][len("round: ") :].strip(),
            "artist_name": lines[2][len("artist: ") :].strip(),
            "title": lines[3][len("title: ") :].strip(),
            "tagline": lines[4][len("tagline: ") :].strip(),
            "concept": lines[5][len("concept: ") :].strip(),
            "prompt_summary": lines[6][len("prompt_summary: ") :].strip(),
            "image_channel_id": lines[7][len("image_channel: ") :].strip(),
            "image_name": lines[8][len("image_name: ") :].strip(),
        },
    )


def sanitize_inline(text: str) -> str:
    return " ".join(text.replace("\n", " ").split())


def sanitize_filename(text: str) -> str:
    cleaned = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "-" for ch in text.strip())
    cleaned = cleaned.strip("-._") or "image"
    return cleaned


def round_output_dir(base_dir: Path, round_id: str) -> Path:
    path = base_dir / "rounds" / round_id
    path.mkdir(parents=True, exist_ok=True)
    return path


def build_export_path(base_dir: Path, round_id: str, artist_name: str, image_name: str) -> Path:
    safe_artist = sanitize_filename(artist_name)
    safe_name = sanitize_filename(image_name)
    return round_output_dir(base_dir, round_id) / f"{safe_artist}-{safe_name}"


def fallback_direction(*, topic: str, persona_name: str, poster_theme: str) -> str:
    return (
        f"Create a {poster_theme} campaign image for '{topic}' in a {persona_name} mode. "
        "Use one dominant focal scene and make it feel like a real launch poster."
    )


def draft_artist_direction(
    *,
    client: OpenAI,
    model: str,
    temperature: float,
    topic: str,
    poster_theme: str,
    artist_name: str,
    persona_name: str,
    persona_brief: str,
) -> str:
    response = client.responses.create(
        **build_responses_kwargs(model=model, temperature=temperature, max_output_tokens=140),
        instructions=(
            "You are a creative director assigning one image campaign brief to a specialist artist. "
            "Write one compact paragraph, no markdown, no bullets, under 85 words."
        ),
        input=(
            f"Topic: {topic}\n"
            f"Artist name: {artist_name}\n"
            f"Poster theme: {poster_theme}\n"
            f"Artist persona: {persona_name}\n"
            f"Persona notes: {persona_brief}\n"
            "Give a vivid direction with one clear focal scene, no visual clutter, and a strong commercial-poster feeling."
        ),
    )
    brief = response.output_text.strip()
    return brief or fallback_direction(topic=topic, persona_name=persona_name, poster_theme=poster_theme)


def draft_image_plan(
    *,
    client: OpenAI,
    model: str,
    temperature: float,
    artist_name: str,
    task: StudioTask,
) -> ImagePlan:
    response = client.responses.create(
        **build_responses_kwargs(model=model, temperature=temperature, max_output_tokens=400),
        instructions=(
            "You are an image-campaign artist preparing a polished submission. "
            "Return exactly this format:\n"
            "TITLE: <short title>\n"
            "TAGLINE: <very short tagline>\n"
            "CONCEPT: <one sentence about the scene>\n"
            "PROMPT_SUMMARY: <one sentence summary of the image prompt>\n"
            "IMAGE_PROMPT:\n"
            "<full image generation prompt>"
        ),
        input=(
            f"Artist name: {artist_name}\n"
            f"Topic: {task.topic}\n"
            f"Persona: {task.persona_name}\n"
            f"Direction: {task.direction}\n"
            "Produce a strong gallery-worthy campaign image concept. "
            "Keep the title and tagline punchy, and write an image prompt for a visually striking horizontal poster image."
        ),
    )
    parsed = parse_image_plan(response.output_text)
    if parsed is not None:
        return parsed
    return ImagePlan(
        title=f"{task.topic.title()} Tonight",
        tagline=f"{task.persona_name} edition",
        concept=f"A dramatic campaign image for {task.topic}.",
        prompt_summary=f"{task.persona_name} campaign image for {task.topic}.",
        image_prompt=(
            f"Create a cinematic horizontal campaign image for '{task.topic}' in a {task.persona_name} style. "
            "One dominant focal subject, clean composition, dramatic lighting, no clutter, launch-poster energy."
        ),
    )


def parse_image_plan(text: str) -> ImagePlan | None:
    lines = text.splitlines()
    if len(lines) < 5:
        return None
    if not lines[0].startswith("TITLE:"):
        return None
    if not lines[1].startswith("TAGLINE:"):
        return None
    if not lines[2].startswith("CONCEPT:"):
        return None
    if not lines[3].startswith("PROMPT_SUMMARY:"):
        return None
    if lines[4].strip() != "IMAGE_PROMPT:":
        return None
    image_prompt = "\n".join(lines[5:]).strip()
    if not image_prompt:
        return None
    return _validate_demo_model(
        ImagePlan,
        {
            "title": lines[0].split(":", 1)[1].strip(),
            "tagline": lines[1].split(":", 1)[1].strip(),
            "concept": lines[2].split(":", 1)[1].strip(),
            "prompt_summary": lines[3].split(":", 1)[1].strip(),
            "image_prompt": image_prompt,
        },
    )


def generate_image_file(
    *,
    client: OpenAI,
    model: str,
    prompt: str,
    size: str,
    quality: str,
    output_path: Path,
) -> Path:
    response = client.images.generate(
        model=model,
        prompt=prompt,
        size=size,
        quality=quality,
        output_format="png",
    )
    if not response.data or not response.data[0].b64_json:
        raise RuntimeError("image generation returned no image data")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_bytes(base64.b64decode(response.data[0].b64_json))
    return output_path


def choose_winner(
    *,
    client: OpenAI,
    model: str,
    temperature: float,
    topic: str,
    submissions: list[SubmissionMetadata],
) -> WinnerDecision:
    response = client.responses.create(
        **build_responses_kwargs(model=model, temperature=temperature, max_output_tokens=120),
        instructions=(
            "You are the witty jury chair for a campaign image competition. "
            "Choose one winner. Return exactly:\n"
            "WINNER: <artist name>\n"
            "REASON: <one witty sentence under 24 words>"
        ),
        input=_format_winner_prompt(topic=topic, submissions=submissions),
    )
    parsed = parse_winner_decision(response.output_text)
    if parsed is not None:
        return parsed
    return WinnerDecision(
        winner_name=submissions[0].artist_name,
        reason="Won by default after the jury spilled espresso on the scorecards.",
    )


def _format_winner_prompt(*, topic: str, submissions: list[SubmissionMetadata]) -> str:
    blocks = [f"Topic: {topic}"]
    for submission in submissions:
        blocks.append(
            "\n".join(
                [
                    f"Artist: {submission.artist_name}",
                    f"Title: {submission.title}",
                    f"Tagline: {submission.tagline}",
                    f"Concept: {submission.concept}",
                    f"Prompt summary: {submission.prompt_summary}",
                ]
            )
        )
    return "\n\n".join(blocks)


def parse_winner_decision(text: str) -> WinnerDecision | None:
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if len(lines) < 2 or not lines[0].startswith("WINNER:") or not lines[1].startswith("REASON:"):
        return None
    return _validate_demo_model(
        WinnerDecision,
        {
            "winner_name": lines[0].split(":", 1)[1].strip(),
            "reason": lines[1].split(":", 1)[1].strip(),
        },
    )


def resolve_winner_name(decision: WinnerDecision, submissions: list[SubmissionMetadata]) -> str:
    normalized = decision.winner_name.casefold()
    for submission in submissions:
        if submission.artist_name.casefold() == normalized:
            return submission.artist_name
    for submission in submissions:
        if normalized in submission.artist_name.casefold():
            return submission.artist_name
    return submissions[0].artist_name


def format_member(member: Member) -> str:
    return f"{member.name} ({member.member_id})"


def print_room_ready(role: str, room: SyncRoom, *, self_member: str, host_member: str) -> None:
    print(f"[{role}] room={room.room_id} self={self_member} host={host_member} argv={' '.join(room.argv)}")


ModelT = TypeVar("ModelT", bound=DemoModel)


def _validate_demo_model(model_cls: type[ModelT], data: dict[str, Any]) -> ModelT | None:
    try:
        return model_cls.model_validate(data)
    except ValidationError:
        return None
