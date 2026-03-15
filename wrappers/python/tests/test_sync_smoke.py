from __future__ import annotations

import uuid

from skyffla import ChannelData, Chat, SyncRoom

from .test_smoke import _find_binary, _skip_if_permission_error, RendezvousStub


def test_sync_wrapper_can_join_chat_and_open_machine_channel() -> None:
    binary = _find_binary()
    server = RendezvousStub()
    server.start()

    room_id = f"python-sync-{uuid.uuid4().hex[:8]}"
    host = SyncRoom.host(room_id, binary=binary, server=server.url, name="host")
    join = None
    try:
        try:
            host.wait_for_welcome()
        except Exception as exc:
            _skip_if_permission_error(exc)
            raise

        server.wait_for_room(room_id)
        join = SyncRoom.join(room_id, binary=binary, server=server.url, name="join")

        join.wait_for_member_snapshot(min_members=2)
        joined = host.wait_for_member_joined(name="join")
        host.send_chat("all", "hello from sync python")
        chat = join.wait_for_chat(text="hello from sync python", from_name="host")
        assert isinstance(chat, Chat)

        channel = host.open_machine_channel("sync-c1", to=joined.member.member_id, name="sync-agent-link")
        join.wait_for_channel_opened(channel_id="sync-c1")
        join.channel("sync-c1").send("sync sketch line")
        data = host.wait_for_channel_data(channel_id=channel.channel_id)
        assert isinstance(data, ChannelData)
        assert data.from_name == "join"
        assert data.body == "sync sketch line"
    finally:
        if join is not None:
            join.close()
        host.close()
        server.stop()
