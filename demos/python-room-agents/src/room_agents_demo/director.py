from __future__ import annotations

import argparse
from collections import OrderedDict

from .shared import (
    build_openai_client,
    choose_style_hint,
    choose_next_artist,
    draft_director_brief,
    format_member,
    load_demo_config,
    make_task_message,
    parse_art_message,
    print_room_ready,
    render_task_message,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the director agent demo.")
    parser.add_argument("room_id")
    parser.add_argument("--server")
    parser.add_argument("--local", action="store_true")
    parser.add_argument("--name", default="director")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    config = load_demo_config()
    client = build_openai_client(config.api_key)

    from skyffla import Chat, ErrorEvent, MemberJoined, MemberLeft, SyncRoom

    with SyncRoom.host(
        args.room_id,
        name=args.name,
        server=args.server,
        local=args.local,
    ) as room:
        welcome = room.wait_for_welcome()
        print_room_ready("director", room, self_member=welcome.self_member, host_member=welcome.host_member)

        snapshot = room.wait_for_member_snapshot(min_members=1)
        known_members: OrderedDict[str, str] = OrderedDict(
            (member.member_id, member.name) for member in snapshot.members
        )
        next_number = 1
        last_assigned_artist: str | None = None
        pending_artist_id: str | None = None
        pending_task_id: str | None = None
        pending_number: int | None = None

        print("members:", ", ".join(known_members.values()))
        print(f"theme: {config.counting_theme}")
        print(f"grid: {config.grid_width}x{config.grid_height}")
        print("waiting for artists...")

        def current_artists() -> list[str]:
            return [member_id for member_id in known_members if member_id != welcome.self_member]

        def dispatch_next_turn() -> None:
            nonlocal next_number, last_assigned_artist, pending_artist_id, pending_task_id, pending_number
            if pending_artist_id is not None:
                return
            artist_ids = current_artists()
            next_artist_id = choose_next_artist(artist_ids, last_assigned_artist)
            if next_artist_id is None:
                return
            member_name = known_members[next_artist_id]

            style_hint = choose_style_hint(next_number - 1)
            try:
                brief = draft_director_brief(
                    client=client,
                    model=config.director_model,
                    number=next_number,
                    width=config.grid_width,
                    height=config.grid_height,
                    counting_theme=config.counting_theme,
                    style_hint=style_hint,
                    artist_name=member_name,
                )
            except Exception as exc:
                brief = (
                    f"Draw the number {next_number} in a {style_hint} style. "
                    f"Fit it cleanly inside a {config.grid_width}x{config.grid_height} terminal grid."
                )
                print(f"[director] OpenAI brief generation failed for {member_name}: {exc}")

            task = make_task_message(
                number=next_number,
                width=config.grid_width,
                height=config.grid_height,
                style_hint=style_hint,
                brief=brief,
            )
            room.send_chat(next_artist_id, render_task_message(task))
            pending_artist_id = next_artist_id
            pending_task_id = task.task_id
            pending_number = next_number
            last_assigned_artist = next_artist_id
            next_number += 1
            print(
                f"[director] sent #{task.number} to {member_name} "
                f"(task={task.task_id}, style={style_hint})"
            )

        dispatch_next_turn()

        while True:
            event = room.recv()
            if isinstance(event, MemberJoined):
                known_members[event.member.member_id] = event.member.name
                print(f"[director] artist joined: {format_member(event.member)}")
                dispatch_next_turn()
            elif isinstance(event, MemberLeft):
                former_name = known_members.pop(event.member_id, event.member_id)
                print(f"[director] member left: {former_name} ({event.member_id})")
                if event.member_id == pending_artist_id:
                    print(f"[director] pending turn for {former_name} was interrupted; reassigning number {pending_number}")
                    pending_artist_id = None
                    pending_task_id = None
                    if pending_number is not None:
                        next_number = pending_number
                        pending_number = None
                    dispatch_next_turn()
            elif isinstance(event, Chat):
                if event.from_ == welcome.self_member:
                    continue
                art = parse_art_message(event.text)
                if art is not None:
                    if event.from_ == pending_artist_id and art.task_id == pending_task_id:
                        line_count = len(art.art.splitlines())
                        print(
                            f"[director] received #{art.number} from {art.artist_name} "
                            f"(task={art.task_id}, lines={line_count})"
                        )
                        pending_artist_id = None
                        pending_task_id = None
                        pending_number = None
                        dispatch_next_turn()
                    else:
                        print(f"[director] out-of-turn art from {art.artist_name} ({art.task_id})")
                else:
                    print(f"[director] chat from {event.from_name}: {event.text}")
            elif isinstance(event, ErrorEvent):
                print(f"[director] error {event.code}: {event.message}")
            else:
                print(f"[director] event: {event}")


if __name__ == "__main__":
    raise SystemExit(main())
