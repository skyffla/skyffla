from __future__ import annotations

import argparse
import queue
import sys
import threading
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path

from .shared import (
    build_export_path,
    build_openai_client,
    choose_artist_personas,
    choose_winner,
    draft_artist_direction,
    format_member,
    load_demo_config,
    make_task_message,
    new_round_id,
    parse_submission_message,
    print_room_ready,
    render_task_message,
    resolve_winner_name,
)
from .web import GalleryState, LiveGalleryServer


@dataclass
class PendingFile:
    member_id: str
    round_id: str
    export_path: Path
    image_url: str


class DirectorConsole:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._prompt_visible = False

    def show_prompt(self) -> None:
        with self._lock:
            sys.stdout.write("\ntopic> ")
            sys.stdout.flush()
            self._prompt_visible = True

    def hide_prompt(self) -> None:
        with self._lock:
            if self._prompt_visible:
                sys.stdout.write("\r\033[K")
                sys.stdout.flush()
                self._prompt_visible = False

    def log(self, message: str, *, redraw_prompt: bool = False) -> None:
        with self._lock:
            if self._prompt_visible:
                sys.stdout.write("\r\033[K")
            sys.stdout.write(f"{message}\n")
            if redraw_prompt:
                sys.stdout.write("topic> ")
                self._prompt_visible = True
            else:
                self._prompt_visible = False
            sys.stdout.flush()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the director agent demo.")
    parser.add_argument("room_id")
    parser.add_argument("--server")
    parser.add_argument("--name", default="director")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    config = load_demo_config()
    client = build_openai_client(config.api_key)
    config.output_dir.mkdir(parents=True, exist_ok=True)

    from skyffla import (
        ChannelFileExported,
        ChannelFileReady,
        ChannelOpened,
        Chat,
        ErrorEvent,
        MemberJoined,
        MemberLeft,
        SyncRoom,
    )

    state = GalleryState()
    gallery = LiveGalleryServer(
        state=state,
        output_dir=config.output_dir,
        host=config.gallery_host,
        port=config.gallery_port,
    )
    gallery.start()
    console = DirectorConsole()

    with SyncRoom.host(
        args.room_id,
        name=args.name,
        server=args.server,
    ) as room:
        try:
            welcome = room.wait_for_welcome()
            print_room_ready("director", room, self_member=welcome.self_member, host_member=welcome.host_member)
            print(f"[director] live gallery: {gallery.url}")

            snapshot = room.wait_for_member_snapshot(min_members=1)
            known_members: OrderedDict[str, str] = OrderedDict(
                (member.member_id, member.name) for member in snapshot.members
            )
            state.set_connected_artists(_artist_names(known_members, welcome.self_member))

            print("members:", ", ".join(known_members.values()))
            print(f"theme: {config.poster_theme}")
            print("artists can join at any time. enter a topic when ready. use /quit to exit.")

            topic_queue: queue.Queue[str] = queue.Queue()
            prompt_ready = threading.Event()
            prompt_ready.set()

            def read_topics() -> None:
                while True:
                    prompt_ready.wait()
                    try:
                        console.show_prompt()
                        topic = sys.stdin.readline()
                        if topic == "":
                            topic_queue.put("/quit")
                            return
                    except EOFError:
                        topic_queue.put("/quit")
                        return
                    except KeyboardInterrupt:
                        topic_queue.put("/quit")
                        return
                    topic_queue.put(topic)

            threading.Thread(target=read_topics, daemon=True).start()

            def current_artists() -> list[str]:
                return [member_id for member_id in known_members if member_id != welcome.self_member]

            def sync_connected_artists() -> None:
                state.set_connected_artists(_artist_names(known_members, welcome.self_member))

            def handle_idle_event(event) -> None:
                if isinstance(event, MemberJoined):
                    known_members[event.member.member_id] = event.member.name
                    sync_connected_artists()
                    console.log(f"[director] artist joined: {format_member(event.member)}", redraw_prompt=True)
                elif isinstance(event, MemberLeft):
                    former_name = known_members.pop(event.member_id, event.member_id)
                    sync_connected_artists()
                    console.log(f"[director] member left: {former_name} ({event.member_id})", redraw_prompt=True)
                elif isinstance(event, ErrorEvent):
                    console.log(f"[director] error {event.code}: {event.message}", redraw_prompt=True)
                elif isinstance(event, Chat) and event.from_ != welcome.self_member:
                    console.log(f"[director] chat from {event.from_name}: {event.text}", redraw_prompt=True)
                elif isinstance(event, ChannelOpened):
                    console.log(
                        f"[director] ignored file from {event.from_name} while idle ({event.channel_id})",
                        redraw_prompt=True,
                    )

            while True:
                try:
                    queued_topic = topic_queue.get_nowait()
                except queue.Empty:
                    queued_topic = None

                if queued_topic is None:
                    try:
                        event = room.recv_until(lambda _: True, timeout=0.25)
                    except TimeoutError:
                        continue
                    handle_idle_event(event)
                    continue

                topic = queued_topic.strip()
                if topic == "/quit":
                    return 0
                if not topic:
                    prompt_ready.set()
                    continue
                console.hide_prompt()
                prompt_ready.clear()

                round_artists = current_artists()
                if not round_artists:
                    console.log("[director] no artists connected; waiting for join events")
                    prompt_ready.set()
                    continue

                round_id = new_round_id()
                participant_names = {member_id: known_members[member_id] for member_id in round_artists}
                personas: dict[str, str] = {}
                pending_meta_by_member = set(round_artists)
                pending_image_by_member = set(round_artists)
                task_ids_by_member: dict[str, str] = {}
                metadata_by_member = {}
                pending_files: dict[str, PendingFile] = {}
                assignments: list[tuple[str, str, str, str]] = []
                persona_assignments = choose_artist_personas(len(round_artists))

                console.log(f"[director] starting round {round_id} for topic: {topic}")

                for member_id, (persona_name, persona_brief) in zip(round_artists, persona_assignments):
                    member_name = participant_names[member_id]
                    personas[member_id] = persona_name
                    try:
                        direction = draft_artist_direction(
                            client=client,
                            model=config.director_model,
                            temperature=config.director_temperature,
                            topic=topic,
                            poster_theme=config.poster_theme,
                            artist_name=member_name,
                            persona_name=persona_name,
                            persona_brief=persona_brief,
                        )
                    except Exception as exc:
                        direction = (
                            f"Create a {config.poster_theme} launch image for '{topic}' in a {persona_name} mode. "
                            "One clear focal scene, no clutter, launch-poster energy."
                        )
                        console.log(f"[director] brief generation failed for {member_name}: {exc}")
                    assignments.append((member_id, member_name, persona_name, direction))

                state.start_round(
                    round_id=round_id,
                    topic=topic,
                    participants=participant_names,
                    personas=personas,
                )

                for member_id, member_name, persona_name, direction in assignments:
                    task = make_task_message(
                        round_id=round_id,
                        topic=topic,
                        persona_name=persona_name,
                        direction=direction,
                    )
                    task_ids_by_member[member_id] = task.task_id
                    direction_preview = direction.replace("\n", " ").strip()
                    if len(direction_preview) > 96:
                        direction_preview = direction_preview[:93].rstrip() + "..."
                    console.log(
                        f"[director] brief for {member_name}: "
                        f"persona={persona_name}; {direction_preview}"
                    )
                    room.send_chat(member_id, render_task_message(task))
                    console.log(
                        f"[director] sent '{topic}' to {member_name} "
                        f"(task={task.task_id}, persona={persona_name})"
                    )

                while pending_meta_by_member or pending_image_by_member:
                    event = room.recv()
                    if isinstance(event, MemberJoined):
                        known_members[event.member.member_id] = event.member.name
                        sync_connected_artists()
                        console.log(f"[director] artist joined: {format_member(event.member)}")
                        console.log(f"[director] {event.member.name} joins next round")
                    elif isinstance(event, MemberLeft):
                        former_name = known_members.pop(event.member_id, event.member_id)
                        sync_connected_artists()
                        console.log(f"[director] member left: {former_name} ({event.member_id})")
                        if event.member_id in pending_meta_by_member:
                            pending_meta_by_member.remove(event.member_id)
                            pending_image_by_member.discard(event.member_id)
                            state.drop_submission(round_id=round_id, member_id=event.member_id)
                    elif isinstance(event, Chat):
                        if event.from_ == welcome.self_member:
                            continue
                        submission = parse_submission_message(event.text)
                        if submission is None:
                            console.log(f"[director] chat from {event.from_name}: {event.text}")
                            continue
                        if (
                            event.from_ not in participant_names
                            or submission.round_id != round_id
                            or submission.task_id != task_ids_by_member.get(event.from_)
                        ):
                            console.log(
                                f"[director] ignored out-of-round submission from "
                                f"{submission.artist_name} (task={submission.task_id})"
                            )
                            continue
                        metadata_by_member[event.from_] = submission
                        pending_meta_by_member.discard(event.from_)
                        state.mark_submission_metadata(
                            round_id=round_id,
                            member_id=event.from_,
                            title=submission.title,
                            tagline=submission.tagline,
                            concept=submission.concept,
                            prompt_summary=submission.prompt_summary,
                        )
                        console.log(f"[director] metadata ready: {submission.artist_name} - {submission.title}")
                    elif isinstance(event, ChannelOpened):
                        if event.from_ not in participant_names or event.kind.value != "file":
                            console.log(f"[director] ignored channel {event.channel_id} from {event.from_name}")
                            continue
                        export_path = build_export_path(
                            config.output_dir,
                            round_id,
                            participant_names[event.from_],
                            event.name or f"{event.channel_id}.png",
                        )
                        image_url = f"/files/rounds/{round_id}/{export_path.name}"
                        pending_files[event.channel_id] = PendingFile(
                            member_id=event.from_,
                            round_id=round_id,
                            export_path=export_path,
                            image_url=image_url,
                        )
                        room.accept_channel(event.channel_id)
                        console.log(f"[director] receiving image from {event.from_name}")
                    elif isinstance(event, ChannelFileReady):
                        pending = pending_files.get(event.channel_id)
                        if pending is None:
                            continue
                        room.export_channel_file(event.channel_id, pending.export_path)
                    elif isinstance(event, ChannelFileExported):
                        pending = pending_files.get(event.channel_id)
                        if pending is None:
                            continue
                        pending_image_by_member.discard(pending.member_id)
                        state.mark_submission_image(
                            round_id=pending.round_id,
                            member_id=pending.member_id,
                            image_url=pending.image_url,
                        )
                        console.log(f"[director] image ready: {participant_names[pending.member_id]}")
                    elif isinstance(event, ErrorEvent):
                        console.log(f"[director] error {event.code}: {event.message}")

                completed = [metadata_by_member[member_id] for member_id in metadata_by_member if member_id in participant_names]
                if not completed:
                    console.log("[director] no completed submissions arrived this round")
                    continue

                try:
                    decision = choose_winner(
                        client=client,
                        model=config.judge_model,
                        temperature=config.judge_temperature,
                        topic=topic,
                        submissions=completed,
                    )
                except Exception as exc:
                    decision = None
                    console.log(f"[director] winner selection failed: {exc}")

                if decision is None:
                    winner_name = completed[0].artist_name
                    jury_reason = "Won by default after the jury misplaced its tiny velvet rosettes."
                else:
                    winner_name = resolve_winner_name(decision, completed)
                    jury_reason = decision.reason

                state.finish_round(round_id=round_id, winner_name=winner_name, jury_reason=jury_reason)
                console.log(f"[director] winner: {winner_name}")
                console.log(f"[director] jury: {jury_reason}")
                prompt_ready.set()
        finally:
            gallery.close()


def _artist_names(known_members: OrderedDict[str, str], self_member: str) -> dict[str, str]:
    return {member_id: name for member_id, name in known_members.items() if member_id != self_member}


if __name__ == "__main__":
    raise SystemExit(main())
