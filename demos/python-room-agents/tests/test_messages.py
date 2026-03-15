from room_agents_demo.shared import (
    TaskMessage,
    choose_next_artist,
    choose_style_key_from_hint,
    frame_ascii_art,
    normalize_ascii_art,
    parse_art_message,
    parse_task_message,
    render_number_art,
    render_art_message,
    render_task_message,
)


def test_task_message_round_trips() -> None:
    task = TaskMessage(
        task_id="task1234",
        number=7,
        width=24,
        height=11,
        style_hint="clean outline",
        brief="Keep it compact and readable.",
    )

    parsed = parse_task_message(render_task_message(task))

    assert parsed == task


def test_art_message_round_trips() -> None:
    payload = render_art_message(
        task_id="task1234",
        number=7,
        artist_name="artist-outline",
        art=" /\\_/\\\\\n( o.o )",
    )

    parsed = parse_art_message(payload)

    assert parsed is not None
    assert parsed.task_id == "task1234"
    assert parsed.number == 7
    assert parsed.artist_name == "artist-outline"
    assert parsed.art == " /\\_/\\\\\n( o.o )"


def test_normalize_ascii_art_trims_empty_lines_and_width() -> None:
    art = normalize_ascii_art("\n\n1234567890\nhello world\n", max_width=5, max_lines=1)

    assert art == "12345"


def test_choose_next_artist_wraps_round_robin() -> None:
    artists = ["a1", "a2", "a3"]

    assert choose_next_artist(artists, None) == "a1"
    assert choose_next_artist(artists, "a1") == "a2"
    assert choose_next_artist(artists, "a3") == "a1"


def test_frame_ascii_art_wraps_output_in_a_box() -> None:
    panel = frame_ascii_art("artist drew #3", "33\n33", width=6)

    assert "+- artist drew #3 -+" in panel
    assert "| 33             |" in panel


def test_render_number_art_makes_readable_block_digit() -> None:
    art = render_number_art(number=12, width=24, height=11, style_key="scoreboard")

    assert "#####" in art
    assert "#" in art


def test_choose_style_key_from_hint_maps_known_styles() -> None:
    assert choose_style_key_from_hint("neon sign glow") == "neon"
    assert choose_style_key_from_hint("chunky arcade cabinet") == "arcade"
    assert choose_style_key_from_hint("ornamental poster type") == "poster"
