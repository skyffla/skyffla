from __future__ import annotations

import asyncio
import json
import os
import re
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from pydantic import ValidationError

from .__about__ import __version__
from .models import (
    AcceptChannel,
    BlobRef,
    ChannelData,
    ChannelKind,
    ChannelOpened,
    Chat,
    CloseChannel,
    ExportChannelFile,
    MachineCommand,
    MachineEvent,
    MACHINE_PROTOCOL_VERSION,
    MemberJoined,
    MemberSnapshot,
    OpenChannel,
    ProtocolVersion,
    RejectChannel,
    RoomWelcome,
    RouteAll,
    RouteMember,
    SendChannelData,
    SendChat,
    SendFile,
    dump_message_json,
    normalize_route,
    parse_machine_event,
)


Predicate = Callable[[MachineEvent], bool]


class SkyfflaError(RuntimeError):
    pass


class SkyfflaProtocolError(SkyfflaError):
    pass


class SkyfflaMachineProtocolMismatch(SkyfflaProtocolError):
    pass


class SkyfflaProcessExited(SkyfflaError):
    pass


class SkyfflaVersionMismatch(SkyfflaError):
    pass


def _default_binary() -> str:
    return os.environ.get("SKYFFLA_BIN", "skyffla")


_VERSION_RE = re.compile(r"\b(\d+\.\d+\.\d+)\b")


def _should_skip_version_check() -> bool:
    value = os.environ.get("SKYFFLA_SKIP_VERSION_CHECK", "").strip().lower()
    return value in {"1", "true", "yes", "on"}


def parse_cli_version(text: str) -> str:
    match = _VERSION_RE.search(text)
    if match is None:
        raise SkyfflaVersionMismatch(f"could not parse skyffla version from output: {text!r}")
    return match.group(1)


async def probe_binary_version(binary: str) -> str:
    process = await asyncio.create_subprocess_exec(
        binary,
        "--version",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    stdout, stderr = await process.communicate()
    if process.returncode != 0:
        raise SkyfflaVersionMismatch(
            f"failed to run {binary!r} --version: {(stderr or stdout).decode('utf-8', errors='replace').strip()}"
        )
    return parse_cli_version(stdout.decode("utf-8", errors="replace").strip())


async def ensure_binary_version(binary: str) -> None:
    if _should_skip_version_check():
        return

    binary_version = await probe_binary_version(binary)
    if binary_version != __version__:
        raise SkyfflaVersionMismatch(
            f"skyffla Python package {__version__} requires matching skyffla binary, got {binary_version} from {binary!r}. "
            "Install the corresponding CLI version or set SKYFFLA_SKIP_VERSION_CHECK=1 to bypass the check."
        )


def ensure_machine_protocol_version(version: ProtocolVersion) -> None:
    if not version.is_compatible_with(MACHINE_PROTOCOL_VERSION):
        raise SkyfflaMachineProtocolMismatch(
            "skyffla machine protocol "
            f"{version} is incompatible with Python wrapper protocol {MACHINE_PROTOCOL_VERSION}. "
            "Wrappers require the same machine protocol major version."
        )


@dataclass(frozen=True)
class MachineChannel:
    room: "Room"
    channel_id: str

    async def send(self, body: str) -> None:
        await self.room.send_channel_data(self.channel_id, body)

    async def accept(self) -> None:
        await self.room.accept_channel(self.channel_id)

    async def reject(self, reason: str | None = None) -> None:
        await self.room.reject_channel(self.channel_id, reason=reason)

    async def close(self, reason: str | None = None) -> None:
        await self.room.close_channel(self.channel_id, reason=reason)

    async def export_file(self, path: str | Path) -> None:
        await self.room.export_channel_file(self.channel_id, path)


class Room:
    def __init__(
        self,
        *,
        role: str,
        room_id: str,
        argv: list[str],
        process: asyncio.subprocess.Process,
    ) -> None:
        self.role = role
        self.room_id = room_id
        self.argv = tuple(argv)
        self._process = process
        self._stdout = process.stdout
        self._stdin = process.stdin
        self._stderr = process.stderr
        self._stderr_lines: list[str] = []
        self._stderr_task = asyncio.create_task(self._collect_stderr())

        if self._stdout is None or self._stdin is None or self._stderr is None:
            raise SkyfflaError("skyffla process must be spawned with stdin/stdout/stderr pipes")

    @classmethod
    async def host(
        cls,
        room_id: str,
        *,
        binary: str | os.PathLike[str] | None = None,
        server: str | None = None,
        name: str | None = None,
        download_dir: str | os.PathLike[str] | None = None,
        local: bool = False,
    ) -> "Room":
        return await cls._spawn(
            "host",
            room_id,
            binary=binary,
            server=server,
            name=name,
            download_dir=download_dir,
            local=local,
        )

    @classmethod
    async def join(
        cls,
        room_id: str,
        *,
        binary: str | os.PathLike[str] | None = None,
        server: str | None = None,
        name: str | None = None,
        download_dir: str | os.PathLike[str] | None = None,
        local: bool = False,
    ) -> "Room":
        return await cls._spawn(
            "join",
            room_id,
            binary=binary,
            server=server,
            name=name,
            download_dir=download_dir,
            local=local,
        )

    @classmethod
    async def _spawn(
        cls,
        role: str,
        room_id: str,
        *,
        binary: str | os.PathLike[str] | None,
        server: str | None,
        name: str | None,
        download_dir: str | os.PathLike[str] | None,
        local: bool,
    ) -> "Room":
        binary_path = os.fspath(binary or _default_binary())
        await ensure_binary_version(binary_path)

        argv = [binary_path, role, room_id, "machine", "--json"]
        if server is not None:
            argv.extend(["--server", server])
        if name is not None:
            argv.extend(["--name", name])
        if download_dir is not None:
            argv.extend(["--download-dir", os.fspath(download_dir)])
        if local:
            argv.append("--local")

        process = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        return cls(role=role, room_id=room_id, argv=argv, process=process)

    def channel(self, channel_id: str) -> MachineChannel:
        return MachineChannel(self, channel_id)

    async def send(self, command: MachineCommand) -> None:
        if self._stdin is None or self._stdin.is_closing():
            raise SkyfflaProcessExited(self._exit_message("stdin is already closed"))

        payload = dump_message_json(command)
        try:
            self._stdin.write(payload.encode("utf-8"))
            self._stdin.write(b"\n")
            await self._stdin.drain()
        except (BrokenPipeError, ConnectionResetError) as exc:
            raise SkyfflaProcessExited(self._exit_message("failed writing command to skyffla")) from exc

    async def send_chat(self, to: str | RouteAll | RouteMember, text: str) -> None:
        await self.send(SendChat(to=normalize_route(to), text=text))

    async def send_file(
        self,
        channel_id: str,
        to: str | RouteAll | RouteMember,
        path: str | Path,
        *,
        name: str | None = None,
        mime: str | None = None,
    ) -> None:
        await self.send(
            SendFile(
                channel_id=channel_id,
                to=normalize_route(to),
                path=os.fspath(path),
                name=name,
                mime=mime,
            )
        )

    async def open_channel(
        self,
        channel_id: str,
        *,
        kind: ChannelKind,
        to: str | RouteAll | RouteMember,
        name: str | None = None,
        size: int | None = None,
        mime: str | None = None,
        blob: BlobRef | None = None,
    ) -> MachineChannel:
        await self.send(
            OpenChannel(
                channel_id=channel_id,
                kind=kind,
                to=normalize_route(to),
                name=name,
                size=size,
                mime=mime,
                blob=blob,
            )
        )
        return self.channel(channel_id)

    async def open_machine_channel(
        self,
        channel_id: str,
        *,
        to: str | RouteAll | RouteMember,
        name: str | None = None,
    ) -> MachineChannel:
        return await self.open_channel(
            channel_id,
            kind=ChannelKind.MACHINE,
            to=to,
            name=name,
        )

    async def accept_channel(self, channel_id: str) -> None:
        await self.send(AcceptChannel(channel_id=channel_id))

    async def reject_channel(self, channel_id: str, *, reason: str | None = None) -> None:
        await self.send(RejectChannel(channel_id=channel_id, reason=reason))

    async def send_channel_data(self, channel_id: str, body: str) -> None:
        await self.send(SendChannelData(channel_id=channel_id, body=body))

    async def close_channel(self, channel_id: str, *, reason: str | None = None) -> None:
        await self.send(CloseChannel(channel_id=channel_id, reason=reason))

    async def export_channel_file(self, channel_id: str, path: str | Path) -> None:
        await self.send(ExportChannelFile(channel_id=channel_id, path=os.fspath(path)))

    async def recv(self) -> MachineEvent:
        if self._stdout is None:
            raise SkyfflaProcessExited(self._exit_message("stdout is already closed"))

        line = await self._stdout.readline()
        if not line:
            raise SkyfflaProcessExited(self._exit_message("skyffla closed stdout"))

        text = line.decode("utf-8").strip()
        if not text:
            return await self.recv()

        try:
            payload = json.loads(text)
        except json.JSONDecodeError as exc:
            raise SkyfflaProtocolError(f"invalid machine event JSON: {text}") from exc

        try:
            event = parse_machine_event(payload)
        except ValidationError as exc:
            raise SkyfflaProtocolError(f"invalid machine event payload: {payload}") from exc
        if isinstance(event, RoomWelcome):
            ensure_machine_protocol_version(event.protocol_version)
        return event

    async def recv_until(self, predicate: Predicate, *, timeout: float = 30.0) -> MachineEvent:
        async def _wait() -> MachineEvent:
            while True:
                event = await self.recv()
                if predicate(event):
                    return event

        return await asyncio.wait_for(_wait(), timeout=timeout)

    async def wait_for_welcome(self, *, timeout: float = 30.0) -> RoomWelcome:
        event = await self.recv_until(lambda item: isinstance(item, RoomWelcome), timeout=timeout)
        assert isinstance(event, RoomWelcome)
        return event

    async def wait_for_member_snapshot(self, *, min_members: int = 1, timeout: float = 30.0) -> MemberSnapshot:
        event = await self.recv_until(
            lambda item: isinstance(item, MemberSnapshot) and len(item.members) >= min_members,
            timeout=timeout,
        )
        assert isinstance(event, MemberSnapshot)
        return event

    async def wait_for_member_joined(self, *, name: str | None = None, timeout: float = 30.0) -> MemberJoined:
        event = await self.recv_until(
            lambda item: isinstance(item, MemberJoined) and (name is None or item.member.name == name),
            timeout=timeout,
        )
        assert isinstance(event, MemberJoined)
        return event

    async def wait_for_chat(
        self,
        *,
        text: str | None = None,
        from_name: str | None = None,
        timeout: float = 30.0,
    ) -> Chat:
        event = await self.recv_until(
            lambda item: isinstance(item, Chat)
            and (text is None or item.text == text)
            and (from_name is None or item.from_name == from_name),
            timeout=timeout,
        )
        assert isinstance(event, Chat)
        return event

    async def wait_for_channel_opened(self, *, channel_id: str | None = None, timeout: float = 30.0) -> ChannelOpened:
        event = await self.recv_until(
            lambda item: isinstance(item, ChannelOpened)
            and (channel_id is None or item.channel_id == channel_id),
            timeout=timeout,
        )
        assert isinstance(event, ChannelOpened)
        return event

    async def wait_for_channel_data(self, *, channel_id: str | None = None, timeout: float = 30.0) -> ChannelData:
        event = await self.recv_until(
            lambda item: isinstance(item, ChannelData)
            and (channel_id is None or item.channel_id == channel_id),
            timeout=timeout,
        )
        assert isinstance(event, ChannelData)
        return event

    async def wait(self) -> int:
        return await self._process.wait()

    def stderr_lines(self) -> tuple[str, ...]:
        return tuple(self._stderr_lines)

    async def aclose(self) -> None:
        if self._stdin is not None and not self._stdin.is_closing():
            self._stdin.close()

        if self._process.returncode is None:
            self._process.terminate()
            try:
                await asyncio.wait_for(self._process.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._process.kill()
                await self._process.wait()

        await self._stderr_task

    async def __aenter__(self) -> "Room":
        return self

    async def __aexit__(self, _exc_type, _exc, _tb) -> None:
        await self.aclose()

    def __aiter__(self) -> "Room":
        return self

    async def __anext__(self) -> MachineEvent:
        try:
            return await self.recv()
        except SkyfflaProcessExited as exc:
            raise StopAsyncIteration from exc

    async def _collect_stderr(self) -> None:
        if self._stderr is None:
            return

        while True:
            line = await self._stderr.readline()
            if not line:
                break
            self._stderr_lines.append(line.decode("utf-8", errors="replace").rstrip())

    def _exit_message(self, detail: str) -> str:
        returncode = self._process.returncode
        stderr = "\n".join(self._stderr_lines[-20:])
        message = f"{detail}; argv={list(self.argv)!r}"
        if returncode is not None:
            message += f"; returncode={returncode}"
        if stderr:
            message += f"\nstderr:\n{stderr}"
        return message
