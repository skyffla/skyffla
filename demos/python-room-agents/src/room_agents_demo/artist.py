from __future__ import annotations

import argparse

from .shared import (
    build_openai_client,
    clear_screen,
    choose_style_key_from_hint,
    frame_ascii_art,
    generate_ascii_art,
    load_demo_config,
    parse_task_message,
    print_room_ready,
    render_number_art,
    render_art_message,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run an artist agent demo.")
    parser.add_argument("room_id")
    parser.add_argument("--server")
    parser.add_argument("--local", action="store_true")
    parser.add_argument("--name", default="artist")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    config = load_demo_config()
    client = build_openai_client(config.api_key)

    from skyffla import Chat, ErrorEvent, MemberJoined, MemberLeft, SyncRoom

    with SyncRoom.join(
        args.room_id,
        name=args.name,
        server=args.server,
        local=args.local,
    ) as room:
        welcome = room.wait_for_welcome()
        print_room_ready("artist", room, self_member=welcome.self_member, host_member=welcome.host_member)

        snapshot = room.wait_for_member_snapshot(min_members=1)
        print("members:", ", ".join(member.name for member in snapshot.members))
        print("waiting for director tasks...")

        handled_tasks: set[str] = set()

        while True:
            event = room.recv()
            if isinstance(event, MemberJoined):
                print(f"[artist] member joined: {event.member.name} ({event.member.member_id})")
            elif isinstance(event, MemberLeft):
                print(f"[artist] member left: {event.member_id}")
            elif isinstance(event, Chat):
                if event.from_ == welcome.self_member:
                    continue
                task = parse_task_message(event.text)
                if task is None:
                    print(f"[artist] chat from {event.from_name}: {event.text}")
                    continue
                if task.task_id in handled_tasks:
                    continue

                handled_tasks.add(task.task_id)
                clear_screen()
                print(
                    f"[artist] received #{task.number} from {event.from_name} "
                    f"(task={task.task_id}, style={task.style_hint})"
                )
                try:
                    art = generate_ascii_art(
                        client=client,
                        model=config.artist_model,
                        artist_name=args.name,
                        task=task,
                    )
                    style_label = "model-selected"
                except Exception as exc:
                    art = render_number_art(
                        number=task.number,
                        width=task.width,
                        height=task.height,
                        style_key=choose_style_key_from_hint(task.style_hint),
                    )
                    style_label = "fallback"
                    print(f"[artist] OpenAI art generation failed: {exc}")

                message = render_art_message(
                    task_id=task.task_id,
                    number=task.number,
                    artist_name=args.name,
                    art=art,
                )
                room.send_chat(welcome.host_member, message)
                print(
                    frame_ascii_art(
                        f"{args.name} painted #{task.number} ({style_label})",
                        art,
                        width=task.width,
                    )
                )
            elif isinstance(event, ErrorEvent):
                print(f"[artist] error {event.code}: {event.message}")
            else:
                print(f"[artist] event: {event}")


if __name__ == "__main__":
    raise SystemExit(main())
