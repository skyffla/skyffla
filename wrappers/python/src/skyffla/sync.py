from __future__ import annotations

import asyncio
import threading
from concurrent.futures import Future
from dataclasses import dataclass
from pathlib import Path

from .client import MachineChannel, Room
from .models import (
    BlobRef,
    ChannelData,
    ChannelKind,
    ChannelOpened,
    Chat,
    MachineEvent,
    MemberJoined,
    MemberSnapshot,
    RoomWelcome,
    RouteAll,
    RouteMember,
)


class _LoopThread:
    def __init__(self) -> None:
        self._ready = threading.Event()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        self._ready.wait()

    def _run(self) -> None:
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        self._loop = loop
        self._ready.set()
        loop.run_forever()
        pending = asyncio.all_tasks(loop)
        for task in pending:
            task.cancel()
        if pending:
            loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
        loop.close()

    def run(self, coro):
        if self._loop is None:
            raise RuntimeError("event loop thread is not ready")
        future: Future = asyncio.run_coroutine_threadsafe(coro, self._loop)
        return future.result()

    def close(self) -> None:
        if self._loop is None:
            return
        self._loop.call_soon_threadsafe(self._loop.stop)
        self._thread.join(timeout=5)
        self._loop = None


@dataclass(frozen=True)
class SyncMachineChannel:
    room: "SyncRoom"
    channel_id: str

    def send(self, body: str) -> None:
        self.room.send_channel_data(self.channel_id, body)

    def accept(self) -> None:
        self.room.accept_channel(self.channel_id)

    def reject(self, reason: str | None = None) -> None:
        self.room.reject_channel(self.channel_id, reason=reason)

    def close(self, reason: str | None = None) -> None:
        self.room.close_channel(self.channel_id, reason=reason)

    def export_file(self, path: str | Path) -> None:
        self.room.export_channel_file(self.channel_id, path)


class SyncRoom:
    def __init__(self, runner: _LoopThread, room: Room) -> None:
        self._runner = runner
        self._room = room
        self.role = room.role
        self.room_id = room.room_id
        self.argv = room.argv

    @classmethod
    def host(
        cls,
        room_id: str,
        *,
        binary: str | Path | None = None,
        server: str | None = None,
        name: str | None = None,
        download_dir: str | Path | None = None,
        local: bool = False,
    ) -> "SyncRoom":
        runner = _LoopThread()
        try:
            room = runner.run(
                Room.host(
                    room_id,
                    binary=binary,
                    server=server,
                    name=name,
                    download_dir=download_dir,
                    local=local,
                )
            )
        except BaseException:
            runner.close()
            raise
        return cls(runner, room)

    @classmethod
    def join(
        cls,
        room_id: str,
        *,
        binary: str | Path | None = None,
        server: str | None = None,
        name: str | None = None,
        download_dir: str | Path | None = None,
        local: bool = False,
    ) -> "SyncRoom":
        runner = _LoopThread()
        try:
            room = runner.run(
                Room.join(
                    room_id,
                    binary=binary,
                    server=server,
                    name=name,
                    download_dir=download_dir,
                    local=local,
                )
            )
        except BaseException:
            runner.close()
            raise
        return cls(runner, room)

    def channel(self, channel_id: str) -> SyncMachineChannel:
        return SyncMachineChannel(self, channel_id)

    def send(self, command) -> None:
        self._runner.run(self._room.send(command))

    def send_chat(self, to: str | RouteAll | RouteMember, text: str) -> None:
        self._runner.run(self._room.send_chat(to, text))

    def send_file(
        self,
        channel_id: str,
        to: str | RouteAll | RouteMember,
        path: str | Path,
        *,
        name: str | None = None,
        mime: str | None = None,
    ) -> None:
        self._runner.run(
            self._room.send_file(
                channel_id,
                to,
                path,
                name=name,
                mime=mime,
            )
        )

    def open_channel(
        self,
        channel_id: str,
        *,
        kind: ChannelKind,
        to: str | RouteAll | RouteMember,
        name: str | None = None,
        size: int | None = None,
        mime: str | None = None,
        blob: BlobRef | None = None,
    ) -> SyncMachineChannel:
        channel: MachineChannel = self._runner.run(
            self._room.open_channel(
                channel_id,
                kind=kind,
                to=to,
                name=name,
                size=size,
                mime=mime,
                blob=blob,
            )
        )
        return SyncMachineChannel(self, channel.channel_id)

    def open_machine_channel(
        self,
        channel_id: str,
        *,
        to: str | RouteAll | RouteMember,
        name: str | None = None,
    ) -> SyncMachineChannel:
        channel: MachineChannel = self._runner.run(
            self._room.open_machine_channel(channel_id, to=to, name=name)
        )
        return SyncMachineChannel(self, channel.channel_id)

    def accept_channel(self, channel_id: str) -> None:
        self._runner.run(self._room.accept_channel(channel_id))

    def reject_channel(self, channel_id: str, *, reason: str | None = None) -> None:
        self._runner.run(self._room.reject_channel(channel_id, reason=reason))

    def send_channel_data(self, channel_id: str, body: str) -> None:
        self._runner.run(self._room.send_channel_data(channel_id, body))

    def close_channel(self, channel_id: str, *, reason: str | None = None) -> None:
        self._runner.run(self._room.close_channel(channel_id, reason=reason))

    def export_channel_file(self, channel_id: str, path: str | Path) -> None:
        self._runner.run(self._room.export_channel_file(channel_id, path))

    def recv(self) -> MachineEvent:
        return self._runner.run(self._room.recv())

    def recv_until(self, predicate, *, timeout: float = 30.0) -> MachineEvent:
        return self._runner.run(self._room.recv_until(predicate, timeout=timeout))

    def wait_for_welcome(self, *, timeout: float = 30.0) -> RoomWelcome:
        event = self.recv_until(lambda item: isinstance(item, RoomWelcome), timeout=timeout)
        assert isinstance(event, RoomWelcome)
        return event

    def wait_for_member_snapshot(self, *, min_members: int = 1, timeout: float = 30.0) -> MemberSnapshot:
        event = self.recv_until(
            lambda item: isinstance(item, MemberSnapshot) and len(item.members) >= min_members,
            timeout=timeout,
        )
        assert isinstance(event, MemberSnapshot)
        return event

    def wait_for_member_joined(self, *, name: str | None = None, timeout: float = 30.0) -> MemberJoined:
        event = self.recv_until(
            lambda item: isinstance(item, MemberJoined) and (name is None or item.member.name == name),
            timeout=timeout,
        )
        assert isinstance(event, MemberJoined)
        return event

    def wait_for_chat(
        self,
        *,
        text: str | None = None,
        from_name: str | None = None,
        timeout: float = 30.0,
    ) -> Chat:
        event = self.recv_until(
            lambda item: isinstance(item, Chat)
            and (text is None or item.text == text)
            and (from_name is None or item.from_name == from_name),
            timeout=timeout,
        )
        assert isinstance(event, Chat)
        return event

    def wait_for_channel_opened(self, *, channel_id: str | None = None, timeout: float = 30.0) -> ChannelOpened:
        event = self.recv_until(
            lambda item: isinstance(item, ChannelOpened)
            and (channel_id is None or item.channel_id == channel_id),
            timeout=timeout,
        )
        assert isinstance(event, ChannelOpened)
        return event

    def wait_for_channel_data(self, *, channel_id: str | None = None, timeout: float = 30.0) -> ChannelData:
        event = self.recv_until(
            lambda item: isinstance(item, ChannelData)
            and (channel_id is None or item.channel_id == channel_id),
            timeout=timeout,
        )
        assert isinstance(event, ChannelData)
        return event

    def stderr_lines(self) -> tuple[str, ...]:
        return self._room.stderr_lines()

    def close(self) -> None:
        try:
            self._runner.run(self._room.aclose())
        finally:
            self._runner.close()

    def __enter__(self) -> "SyncRoom":
        return self

    def __exit__(self, _exc_type, _exc, _tb) -> None:
        self.close()
