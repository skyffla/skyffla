from __future__ import annotations

import argparse

from .shared import (
    build_openai_client,
    draft_image_plan,
    format_member,
    generate_image_file,
    load_demo_config,
    new_channel_id,
    parse_task_message,
    print_room_ready,
    render_submission_message,
    round_output_dir,
    sanitize_filename,
    SubmissionMetadata,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run an artist agent demo.")
    parser.add_argument("room_id")
    parser.add_argument("--server")
    parser.add_argument("--name", default="artist")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    config = load_demo_config()
    client = build_openai_client(config.api_key)

    from skyffla import ChannelAccepted, Chat, ErrorEvent, MemberJoined, MemberLeft, SyncRoom, TransferProgress

    with SyncRoom.join(
        args.room_id,
        name=args.name,
        server=args.server,
    ) as room:
        welcome = room.wait_for_welcome()
        snapshot = room.wait_for_member_snapshot(min_members=1)

        print_room_ready(
            "artist",
            room,
            self_member=welcome.self_member,
            host_member=welcome.host_member,
        )
        print("members:", ", ".join(member.name for member in snapshot.members))
        print("waiting for director topics...")

        handled_tasks: set[str] = set()

        while True:
            event = room.recv()
            if isinstance(event, MemberJoined):
                continue
            elif isinstance(event, MemberLeft):
                continue
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
                print(
                    f"[artist] received topic '{task.topic}' from {event.from_name} "
                    f"(task={task.task_id}, persona={task.persona_name})"
                )

                try:
                    plan = draft_image_plan(
                        client=client,
                        model=config.artist_model,
                        temperature=config.artist_temperature,
                        artist_name=args.name,
                        task=task,
                    )
                except Exception as exc:
                    print(f"[artist] concept generation failed: {exc}")
                    continue

                local_round_dir = round_output_dir(
                    config.output_dir / "artist-cache" / sanitize_filename(args.name),
                    task.round_id,
                )
                image_name = f"{sanitize_filename(plan.title)}.png"
                image_path = local_round_dir / image_name

                try:
                    generate_image_file(
                        client=client,
                        model=config.image_model,
                        prompt=plan.image_prompt,
                        size=config.image_size,
                        quality=config.image_quality,
                        output_path=image_path,
                    )
                except Exception as exc:
                    print(f"[artist] image generation failed: {exc}")
                    continue

                channel_id = new_channel_id()
                metadata = SubmissionMetadata(
                    task_id=task.task_id,
                    round_id=task.round_id,
                    artist_name=args.name,
                    title=plan.title,
                    tagline=plan.tagline,
                    concept=plan.concept,
                    prompt_summary=plan.prompt_summary,
                    image_channel_id=channel_id,
                    image_name=image_name,
                )
                room.send_chat(welcome.host_member, render_submission_message(metadata))
                room.send_file(
                    channel_id,
                    welcome.host_member,
                    image_path,
                    name=image_name,
                    mime="image/png",
                )

                print(f"[artist] created: {plan.title}")
                print(f"[artist] tagline: {plan.tagline}")
                print(f"[artist] sent image")
            elif isinstance(event, TransferProgress):
                continue
            elif isinstance(event, ChannelAccepted):
                print(f"[artist] upload accepted by {event.member_name}")
            elif isinstance(event, ErrorEvent):
                print(f"[artist] error {event.code}: {event.message}")
            else:
                continue

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
