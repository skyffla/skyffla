from __future__ import annotations

import sys

from skyffla import SyncRoom


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: python examples/sync_chat_and_channel.py <host|join> <room-id>")
        return 2

    role, room_id = sys.argv[1], sys.argv[2]
    room_cls = SyncRoom.host if role == "host" else SyncRoom.join

    with room_cls(room_id, name=f"python-{role}") as room:
        welcome = room.wait_for_welcome()
        print(f"joined room={welcome.room_id} self={welcome.self_member} host={welcome.host_member}")

        snapshot = room.wait_for_member_snapshot()
        print("members:", ", ".join(member.name for member in snapshot.members))

        room.send_chat("all", f"hello from {role}")
        print("chat event:", room.wait_for_chat())

        if role == "host":
            joined = room.wait_for_member_joined(timeout=60.0)
            room.open_machine_channel("py-demo", to=joined.member.member_id, name="python-demo")
            print("channel opened:", room.wait_for_channel_data(channel_id="py-demo", timeout=60.0))
        else:
            opened = room.wait_for_channel_opened(channel_id="py-demo", timeout=60.0)
            room.channel(opened.channel_id).send("hello over machine channel")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
