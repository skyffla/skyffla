from __future__ import annotations

import asyncio
import json
import os
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib import error, request

import pytest

from skyffla import (
    ChannelData,
    ChannelOpened,
    Chat,
    MemberJoined,
    MemberSnapshot,
    Room,
    RoomWelcome,
    SkyfflaProcessExited,
)

RENDEZVOUS_VERSION_HEADER = "x-skyffla-rendezvous-version"
RENDEZVOUS_API_VERSION = "1.0"


def _find_binary() -> str:
    configured = os.environ.get("SKYFFLA_BIN")
    if configured:
        return configured

    default = Path(__file__).resolve().parents[3] / "target" / "debug" / "skyffla"
    if default.exists():
        return os.fspath(default)

    pytest.skip("set SKYFFLA_BIN or build target/debug/skyffla to run smoke tests")


def _skip_if_permission_error(exc: SkyfflaProcessExited) -> None:
    message = str(exc)
    if "Operation not permitted" in message or "permission denied" in message:
        pytest.skip("local environment does not allow Skyffla transport sockets")


class RendezvousStub(ThreadingHTTPServer):
    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), RendezvousHandler)
        self.rooms: dict[str, dict[str, object]] = {}
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        host, port = self.server_address
        return f"http://{host}:{port}"

    def start(self) -> None:
        self.thread.start()
        deadline = time.time() + 2
        while time.time() < deadline:
            try:
                with request.urlopen(f"{self.url}/health", timeout=0.2) as response:
                    if response.status == 200:
                        return
            except error.URLError:
                time.sleep(0.05)
        raise RuntimeError("rendezvous stub did not become ready")

    def stop(self) -> None:
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=2)

    def wait_for_room(self, room_id: str, timeout: float = 5.0) -> None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                if room_id in self.rooms:
                    return
            time.sleep(0.05)
        raise TimeoutError(f"room {room_id} was not registered")


class RendezvousHandler(BaseHTTPRequestHandler):
    server: RendezvousStub

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._send_json(200, {"status": "ok"})
            return

        room_id = self._room_id()
        if room_id is None:
            self.send_error(404)
            return

        now = int(time.time())
        with self.server.lock:
            record = self.server.rooms.get(room_id)
            if record is None or int(record["expires_at_epoch_seconds"]) <= now:
                self.server.rooms.pop(room_id, None)
                self.send_error(404)
                return
            payload = {
                "room_id": room_id,
                "ticket": record["ticket"],
                "expires_at_epoch_seconds": record["expires_at_epoch_seconds"],
                "capabilities": record["capabilities"],
            }
        self._send_json(200, payload)

    def do_PUT(self) -> None:  # noqa: N802
        room_id = self._room_id()
        if room_id is None:
            self.send_error(404)
            return

        content_length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(content_length).decode("utf-8"))
        expires_at = int(time.time()) + int(payload["ttl_seconds"])

        with self.server.lock:
            existing = self.server.rooms.get(room_id)
            if existing is not None and existing["ticket"] != payload["ticket"]:
                self._send_json(
                    409,
                    {
                        "error": "room_already_exists",
                        "message": f"room {room_id} is already hosted",
                    },
                )
                return
            self.server.rooms[room_id] = {
                "ticket": payload["ticket"],
                "capabilities": payload["capabilities"],
                "expires_at_epoch_seconds": expires_at,
            }

        self._send_json(
            200,
            {
                "room_id": room_id,
                "expires_at_epoch_seconds": expires_at,
                "status": "waiting",
            },
        )

    def do_DELETE(self) -> None:  # noqa: N802
        room_id = self._room_id()
        if room_id is None:
            self.send_error(404)
            return

        with self.server.lock:
            self.server.rooms.pop(room_id, None)
        self.send_response(204)
        self.send_header(RENDEZVOUS_VERSION_HEADER, RENDEZVOUS_API_VERSION)
        self.end_headers()

    def log_message(self, format: str, *args) -> None:  # noqa: A003
        return

    def _room_id(self) -> str | None:
        parts = self.path.strip("/").split("/")
        if len(parts) == 3 and parts[:2] == ["v1", "rooms"]:
            return parts[2]
        return None

    def _send_json(self, status: int, payload: dict[str, object]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header(RENDEZVOUS_VERSION_HEADER, RENDEZVOUS_API_VERSION)
        self.end_headers()
        self.wfile.write(body)


async def _wait_for_member_count(room: Room, count: int) -> MemberSnapshot:
    event = await room.recv_until(
        lambda item: isinstance(item, MemberSnapshot) and len(item.members) >= count,
        timeout=30.0,
    )
    assert isinstance(event, MemberSnapshot)
    return event


@pytest.mark.asyncio
async def test_wrapper_can_join_chat_and_open_machine_channel() -> None:
    binary = _find_binary()
    server = RendezvousStub()
    server.start()

    room_id = f"python-smoke-{uuid.uuid4().hex[:8]}"
    host = await Room.host(room_id, binary=binary, server=server.url, name="host")
    join = None
    try:
        try:
            welcome = await host.recv_until(
                lambda event: isinstance(event, RoomWelcome),
                timeout=30.0,
            )
        except SkyfflaProcessExited as exc:
            _skip_if_permission_error(exc)
            raise
        assert isinstance(welcome, RoomWelcome)

        server.wait_for_room(room_id)
        join = await Room.join(room_id, binary=binary, server=server.url, name="join")
        try:
            await _wait_for_member_count(join, 2)
            joined = await host.recv_until(
                lambda event: isinstance(event, MemberJoined) and event.member.name == "join",
                timeout=30.0,
            )
            assert isinstance(joined, MemberJoined)
            join_member_id = joined.member.member_id

            await host.send_chat("all", "hello from python")
            chat = await join.recv_until(
                lambda event: isinstance(event, Chat) and event.text == "hello from python",
                timeout=30.0,
            )
            assert isinstance(chat, Chat)
            assert chat.from_name == "host"

            channel = await host.open_machine_channel("c1", to=join_member_id, name="agent-link")
            opened = await join.recv_until(
                lambda event: isinstance(event, ChannelOpened) and event.channel_id == "c1",
                timeout=30.0,
            )
            assert isinstance(opened, ChannelOpened)
            await join.channel("c1").send("sketch line")
            data = await host.recv_until(
                lambda event: isinstance(event, ChannelData) and event.channel_id == channel.channel_id,
                timeout=30.0,
            )
            assert isinstance(data, ChannelData)
            assert data.from_name == "join"
            assert data.body == "sketch line"
        except SkyfflaProcessExited as exc:
            _skip_if_permission_error(exc)
            raise
    finally:
        if join is not None:
            await join.aclose()
        await host.aclose()
        server.stop()
