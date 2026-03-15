from pathlib import Path
import random

from skyffla_examples.studio_app.shared import (
    ImagePlan,
    StudioTask,
    SubmissionMetadata,
    WinnerDecision,
    ARTIST_PERSONAS,
    build_export_path,
    build_responses_kwargs,
    choose_artist_personas,
    new_channel_id,
    parse_image_plan,
    parse_submission_message,
    parse_task_message,
    parse_winner_decision,
    render_submission_message,
    render_task_message,
    resolve_winner_name,
    sanitize_filename,
)
from skyffla_examples.studio_app.web import GalleryState


def test_task_message_round_trips() -> None:
    task = StudioTask(
        task_id="task1234",
        round_id="round42",
        topic="forbidden aquarium grand opening",
        persona_name="hero-silhouette",
        direction="Make the giant tank window the focal scene.",
    )

    assert parse_task_message(render_task_message(task)) == task


def test_submission_message_round_trips() -> None:
    metadata = SubmissionMetadata(
        task_id="task1234",
        round_id="round42",
        artist_name="artist-outline",
        title="Forbidden Aquarium",
        tagline="Tickets vanish at midnight.",
        concept="A secret aquarium launch hidden behind ceremonial glass.",
        prompt_summary="Giant lit tank, secretive crowd, campaign-poster mood.",
        image_channel_id="img-1234abcd",
        image_name="forbidden-aquarium.png",
    )

    assert parse_submission_message(render_submission_message(metadata)) == metadata


def test_choose_artist_personas_shuffles_one_cycle() -> None:
    personas = choose_artist_personas(4, rng=random.Random(7))

    assert len(personas) == 4
    assert len({name for name, _ in personas}) == 4
    assert all(persona in ARTIST_PERSONAS for persona in personas)


def test_choose_artist_personas_repeats_after_full_cycle() -> None:
    personas = choose_artist_personas(len(ARTIST_PERSONAS) + 2, rng=random.Random(3))

    assert len(personas) == len(ARTIST_PERSONAS) + 2
    assert set(personas[: len(ARTIST_PERSONAS)]) == set(ARTIST_PERSONAS)
    assert all(persona in ARTIST_PERSONAS for persona in personas)


def test_build_responses_kwargs_skips_temperature_for_gpt5() -> None:
    kwargs = build_responses_kwargs(model="gpt-5.4", temperature=1.4, max_output_tokens=200)

    assert kwargs["model"] == "gpt-5.4"
    assert "temperature" not in kwargs


def test_parse_image_plan_reads_expected_format() -> None:
    parsed = parse_image_plan(
        "\n".join(
            [
                "TITLE: Neon Reef",
                "TAGLINE: Forbidden after dark.",
                "CONCEPT: A glowing launch poster for a suspicious aquarium opening.",
                "PROMPT_SUMMARY: Lit tank window, elegant crowd, high-contrast campaign image.",
                "IMAGE_PROMPT:",
                "Cinematic horizontal poster image of a secret aquarium gala.",
            ]
        )
    )

    assert parsed == ImagePlan(
        title="Neon Reef",
        tagline="Forbidden after dark.",
        concept="A glowing launch poster for a suspicious aquarium opening.",
        prompt_summary="Lit tank window, elegant crowd, high-contrast campaign image.",
        image_prompt="Cinematic horizontal poster image of a secret aquarium gala.",
    )


def test_parse_winner_decision_reads_two_line_format() -> None:
    parsed = parse_winner_decision(
        "WINNER: artist-shading\nREASON: It feels like contraband luxury with excellent lighting."
    )

    assert parsed == WinnerDecision(
        winner_name="artist-shading",
        reason="It feels like contraband luxury with excellent lighting.",
    )


def test_resolve_winner_name_prefers_named_artist() -> None:
    submissions = [
        SubmissionMetadata(
            task_id="task1",
            round_id="r1",
            artist_name="artist-outline",
            title="A",
            tagline="A",
            concept="A",
            prompt_summary="A",
            image_channel_id="img-a",
            image_name="a.png",
        ),
        SubmissionMetadata(
            task_id="task2",
            round_id="r1",
            artist_name="artist-shading",
            title="B",
            tagline="B",
            concept="B",
            prompt_summary="B",
            image_channel_id="img-b",
            image_name="b.png",
        ),
    ]

    winner = resolve_winner_name(
        WinnerDecision(winner_name="artist-shading", reason="clear winner"),
        submissions,
    )

    assert winner == "artist-shading"


def test_build_export_path_uses_round_folder() -> None:
    path = build_export_path(Path("/tmp/demo-output"), "round42", "artist-shading", "reef poster.png")

    assert path == Path("/tmp/demo-output/rounds/round42/artist-shading-reef-poster.png")


def test_sanitize_filename_normalizes_text() -> None:
    assert sanitize_filename("Forbidden Aquarium!.png") == "Forbidden-Aquarium-.png".strip("-._")
    assert new_channel_id("img").startswith("img-")


def test_gallery_state_tracks_round_progress() -> None:
    state = GalleryState()
    state.set_connected_artists({"m2": "artist-1", "m3": "artist-2"})
    state.start_round(
        round_id="round42",
        topic="forbidden aquarium grand opening",
        participants={"m2": "artist-1", "m3": "artist-2"},
        personas={"m2": "hero-silhouette", "m3": "noir-atmosphere"},
    )
    state.mark_submission_metadata(
        round_id="round42",
        member_id="m2",
        title="Forbidden Aquarium",
        tagline="After dark only.",
        concept="A luxury launch behind forbidden glass.",
        prompt_summary="Tank window, gala crowd, cinematic mood.",
    )
    state.mark_submission_image(
        round_id="round42",
        member_id="m2",
        image_url="/files/rounds/round42/artist-1-reef.png",
    )
    state.finish_round(
        round_id="round42",
        winner_name="artist-1",
        jury_reason="It looks expensive and mildly illegal.",
    )

    snapshot = state.snapshot()

    assert snapshot["current_round"]["winner_name"] == "artist-1"
    assert snapshot["current_round"]["submissions"]["m2"]["status"] == "ready"
    assert snapshot["current_round"]["submissions"]["m2"]["image_url"] == "/files/rounds/round42/artist-1-reef.png"
