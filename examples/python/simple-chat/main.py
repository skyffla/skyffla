from __future__ import annotations

import argparse
import sys
import threading

from skyffla import Chat, ErrorEvent, MemberJoined, MemberLeft, SyncRoom


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("role", choices=["host", "join"])
    parser.add_argument("room_id")
    parser.add_argument("--server")
    parser.add_argument("--local", action="store_true")
    parser.add_argument("--name")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    room_factory = SyncRoom.host if args.role == "host" else SyncRoom.join

    with room_factory(
        args.room_id,
        name=args.name or f"python-{args.role}",
        server=args.server,
        local=args.local,
    ) as room:
        welcome = room.wait_for_welcome()
        print(
            f"connected room={welcome.room_id} self={welcome.self_member} host={welcome.host_member}"
        )

        if args.role == "join" and welcome.self_member == welcome.host_member:
            print(
                "[system] no existing host was found; this join promoted itself to host"
            )

        snapshot = room.wait_for_member_snapshot(min_members=1)
        print("members:", ", ".join(member.name for member in snapshot.members))
        if len(snapshot.members) == 1:
            print("[system] waiting for another member to join")
        print("type a message and press Enter. use /quit to exit.")

        stop = threading.Event()

        def recv_loop() -> None:
            while not stop.is_set():
                try:
                    event = room.recv()
                except Exception as exc:
                    if not stop.is_set():
                        print(f"[error] {exc}")
                    stop.set()
                    return

                if isinstance(event, Chat):
                    print(f"[{event.from_name}] {event.text}")
                elif isinstance(event, MemberJoined):
                    print(f"[system] {event.member.name} joined")
                elif isinstance(event, MemberLeft):
                    print(f"[system] {event.member_id} left")
                elif isinstance(event, ErrorEvent):
                    print(f"[error] {event.code}: {event.message}")
                else:
                    print(f"[event] {event}")

        reader = threading.Thread(target=recv_loop, daemon=True)
        reader.start()

        try:
            for line in sys.stdin:
                text = line.rstrip("\n")
                if stop.is_set():
                    break
                if text == "/quit":
                    break
                if not text.strip():
                    continue
                room.send_chat("all", text)
        finally:
            stop.set()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
