from __future__ import annotations

import json
from enum import Enum
from typing import Any, Literal, Mapping, Union

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, field_validator, model_validator
from typing_extensions import Annotated


def _require_non_empty(value: str, field_name: str) -> str:
    if not value.strip():
        raise ValueError(f"{field_name} must not be empty")
    return value


def _require_non_empty_body(value: str) -> str:
    if value == "":
        raise ValueError("body must not be empty")
    return value


class SkyfflaModel(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)


class ChannelKind(str, Enum):
    MACHINE = "machine"
    FILE = "file"
    CLIPBOARD = "clipboard"
    PIPE = "pipe"


class BlobFormat(str, Enum):
    BLOB = "blob"
    COLLECTION = "collection"


class TransferItemKind(str, Enum):
    FILE = "file"
    FOLDER = "folder"


class TransferPhase(str, Enum):
    PREPARING = "preparing"
    DOWNLOADING = "downloading"
    EXPORTING = "exporting"


class ProtocolVersion(SkyfflaModel):
    major: int
    minor: int

    @field_validator("major")
    @classmethod
    def _validate_major(cls, value: int) -> int:
        if value <= 0:
            raise ValueError("protocol_version.major must be positive")
        return value

    @field_validator("minor")
    @classmethod
    def _validate_minor(cls, value: int) -> int:
        if value < 0:
            raise ValueError("protocol_version.minor must not be negative")
        return value

    def is_compatible_with(self, other: "ProtocolVersion") -> bool:
        return self.major == other.major

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}"


MACHINE_PROTOCOL_VERSION = ProtocolVersion(major=2, minor=1)


class BlobRef(SkyfflaModel):
    hash: str
    format: BlobFormat

    @field_validator("hash")
    @classmethod
    def _validate_hash(cls, value: str) -> str:
        return _require_non_empty(value, "blob_hash")


class TransferDigest(SkyfflaModel):
    algorithm: str
    value: str

    @field_validator("algorithm", "value")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")


class TransferOffer(SkyfflaModel):
    item_kind: TransferItemKind
    integrity: TransferDigest | None = None


class Member(SkyfflaModel):
    member_id: str
    name: str
    fingerprint: str | None = None

    @field_validator("member_id")
    @classmethod
    def _validate_member_id(cls, value: str) -> str:
        return _require_non_empty(value, "member_id")

    @field_validator("name")
    @classmethod
    def _validate_name(cls, value: str) -> str:
        return _require_non_empty(value, "member_name")


class RouteAll(SkyfflaModel):
    type: Literal["all"] = "all"


class RouteMember(SkyfflaModel):
    type: Literal["member"] = "member"
    member_id: str

    @field_validator("member_id")
    @classmethod
    def _validate_member_id(cls, value: str) -> str:
        return _require_non_empty(value, "member_id")


Route = Annotated[Union[RouteAll, RouteMember], Field(discriminator="type")]


def route_all() -> RouteAll:
    return RouteAll()


def route_member(member_id: str) -> RouteMember:
    return RouteMember(member_id=member_id)


def normalize_route(route: str | RouteAll | RouteMember) -> Route:
    if isinstance(route, (RouteAll, RouteMember)):
        return route
    if route == "all":
        return route_all()
    return route_member(route)


class SendChat(SkyfflaModel):
    type: Literal["send_chat"] = "send_chat"
    to: Route
    text: str

    @field_validator("text")
    @classmethod
    def _validate_text(cls, value: str) -> str:
        return _require_non_empty(value, "text")


class SendFile(SkyfflaModel):
    type: Literal["send_file"] = "send_file"
    channel_id: str
    to: Route
    path: str
    name: str | None = None
    mime: str | None = None

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")

    @field_validator("path")
    @classmethod
    def _validate_path(cls, value: str) -> str:
        return _require_non_empty(value, "path")


class OpenChannel(SkyfflaModel):
    type: Literal["open_channel"] = "open_channel"
    channel_id: str
    kind: ChannelKind
    to: Route
    name: str | None = None
    size: int | None = None
    mime: str | None = None
    blob: BlobRef | None = None
    transfer: TransferOffer | None = None

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")

    @model_validator(mode="after")
    def _validate_blob(self) -> OpenChannel:
        if self.kind == ChannelKind.FILE and self.blob is None:
            if self.transfer is None:
                raise ValueError("file channels require blob or transfer metadata")
        if self.kind != ChannelKind.FILE:
            if self.blob is not None:
                raise ValueError("blob metadata is only allowed for file channels")
            if self.transfer is not None:
                raise ValueError("transfer metadata is only allowed for file channels")
        return self


class AcceptChannel(SkyfflaModel):
    type: Literal["accept_channel"] = "accept_channel"
    channel_id: str

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")


class RejectChannel(SkyfflaModel):
    type: Literal["reject_channel"] = "reject_channel"
    channel_id: str
    reason: str | None = None

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")


class SendChannelData(SkyfflaModel):
    type: Literal["send_channel_data"] = "send_channel_data"
    channel_id: str
    body: str

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")

    @field_validator("body")
    @classmethod
    def _validate_body(cls, value: str) -> str:
        return _require_non_empty_body(value)


class CloseChannel(SkyfflaModel):
    type: Literal["close_channel"] = "close_channel"
    channel_id: str
    reason: str | None = None

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")


class ExportChannelFile(SkyfflaModel):
    type: Literal["export_channel_file"] = "export_channel_file"
    channel_id: str
    path: str

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")

    @field_validator("path")
    @classmethod
    def _validate_path(cls, value: str) -> str:
        return _require_non_empty(value, "path")


MachineCommand = Annotated[
    Union[
        SendChat,
        SendFile,
        OpenChannel,
        AcceptChannel,
        RejectChannel,
        SendChannelData,
        CloseChannel,
        ExportChannelFile,
    ],
    Field(discriminator="type"),
]


class RoomWelcome(SkyfflaModel):
    type: Literal["room_welcome"] = "room_welcome"
    protocol_version: ProtocolVersion
    room_id: str
    self_member: str
    host_member: str

    @field_validator("room_id", "self_member", "host_member")
    @classmethod
    def _validate_ids(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name)


class MemberSnapshot(SkyfflaModel):
    type: Literal["member_snapshot"] = "member_snapshot"
    members: list[Member]

    @model_validator(mode="after")
    def _validate_members(self) -> MemberSnapshot:
        if not self.members:
            raise ValueError("member snapshot must not be empty")
        return self


class MemberJoined(SkyfflaModel):
    type: Literal["member_joined"] = "member_joined"
    member: Member


class MemberLeft(SkyfflaModel):
    type: Literal["member_left"] = "member_left"
    member_id: str
    reason: str | None = None

    @field_validator("member_id")
    @classmethod
    def _validate_member_id(cls, value: str) -> str:
        return _require_non_empty(value, "member_id")


class Chat(SkyfflaModel):
    type: Literal["chat"] = "chat"
    from_: str = Field(alias="from")
    from_name: str
    to: Route
    text: str

    @field_validator("from_", "from_name")
    @classmethod
    def _validate_sender_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")

    @field_validator("text")
    @classmethod
    def _validate_text(cls, value: str) -> str:
        return _require_non_empty(value, "text")


class ChannelOpened(SkyfflaModel):
    type: Literal["channel_opened"] = "channel_opened"
    channel_id: str
    kind: ChannelKind
    from_: str = Field(alias="from")
    from_name: str
    to: Route
    name: str | None = None
    size: int | None = None
    mime: str | None = None
    blob: BlobRef | None = None
    transfer: TransferOffer | None = None

    @field_validator("channel_id", "from_", "from_name")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")

    @model_validator(mode="after")
    def _validate_blob(self) -> ChannelOpened:
        if self.kind == ChannelKind.FILE and self.blob is None:
            if self.transfer is None:
                raise ValueError("file channels require blob or transfer metadata")
        if self.kind != ChannelKind.FILE:
            if self.blob is not None:
                raise ValueError("blob metadata is only allowed for file channels")
            if self.transfer is not None:
                raise ValueError("transfer metadata is only allowed for file channels")
        return self


class ChannelAccepted(SkyfflaModel):
    type: Literal["channel_accepted"] = "channel_accepted"
    channel_id: str
    member_id: str
    member_name: str

    @field_validator("channel_id", "member_id", "member_name")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")


class ChannelRejected(SkyfflaModel):
    type: Literal["channel_rejected"] = "channel_rejected"
    channel_id: str
    member_id: str
    member_name: str
    reason: str | None = None

    @field_validator("channel_id", "member_id", "member_name")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")


class ChannelData(SkyfflaModel):
    type: Literal["channel_data"] = "channel_data"
    channel_id: str
    from_: str = Field(alias="from")
    from_name: str
    body: str

    @field_validator("channel_id", "from_", "from_name")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")

    @field_validator("body")
    @classmethod
    def _validate_body(cls, value: str) -> str:
        return _require_non_empty_body(value)


class ChannelClosed(SkyfflaModel):
    type: Literal["channel_closed"] = "channel_closed"
    channel_id: str
    member_id: str
    member_name: str
    reason: str | None = None

    @field_validator("channel_id", "member_id", "member_name")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")


class ChannelFileReady(SkyfflaModel):
    type: Literal["channel_file_ready"] = "channel_file_ready"
    channel_id: str
    blob: BlobRef

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")


class ChannelFileExported(SkyfflaModel):
    type: Literal["channel_file_exported"] = "channel_file_exported"
    channel_id: str
    path: str
    size: int

    @field_validator("channel_id")
    @classmethod
    def _validate_channel_id(cls, value: str) -> str:
        return _require_non_empty(value, "channel_id")

    @field_validator("path")
    @classmethod
    def _validate_path(cls, value: str) -> str:
        return _require_non_empty(value, "path")


class TransferProgress(SkyfflaModel):
    type: Literal["transfer_progress"] = "transfer_progress"
    channel_id: str
    item_kind: TransferItemKind
    name: str
    phase: TransferPhase
    bytes_complete: int
    bytes_total: int | None = None

    @field_validator("channel_id", "name")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")

    @field_validator("bytes_complete")
    @classmethod
    def _validate_bytes_complete(cls, value: int) -> int:
        if value < 0:
            raise ValueError("bytes_complete must not be negative")
        return value

    @field_validator("bytes_total")
    @classmethod
    def _validate_bytes_total(cls, value: int | None) -> int | None:
        if value is not None and value < 0:
            raise ValueError("bytes_total must not be negative")
        return value


class ErrorEvent(SkyfflaModel):
    type: Literal["error"] = "error"
    code: str
    message: str
    channel_id: str | None = None

    @field_validator("code", "message")
    @classmethod
    def _validate_required_fields(cls, value: str, info: Any) -> str:
        return _require_non_empty(value, info.field_name or "field")


MachineEvent = Annotated[
    Union[
        RoomWelcome,
        MemberSnapshot,
        MemberJoined,
        MemberLeft,
        Chat,
        ChannelOpened,
        ChannelAccepted,
        ChannelRejected,
        ChannelData,
        ChannelClosed,
        ChannelFileReady,
        ChannelFileExported,
        TransferProgress,
        ErrorEvent,
    ],
    Field(discriminator="type"),
]

_MACHINE_COMMAND_ADAPTER = TypeAdapter(MachineCommand)
_MACHINE_EVENT_ADAPTER = TypeAdapter(MachineEvent)
_ROUTE_ADAPTER = TypeAdapter(Route)


def parse_route(value: Mapping[str, Any] | RouteAll | RouteMember) -> Route:
    return _ROUTE_ADAPTER.validate_python(value)


def parse_machine_command(value: Mapping[str, Any] | str) -> MachineCommand:
    if isinstance(value, str):
        value = json.loads(value)
    return _MACHINE_COMMAND_ADAPTER.validate_python(value)


def parse_machine_event(value: Mapping[str, Any] | str) -> MachineEvent:
    if isinstance(value, str):
        value = json.loads(value)
    return _MACHINE_EVENT_ADAPTER.validate_python(value)


def dump_message(model: SkyfflaModel) -> dict[str, Any]:
    return model.model_dump(mode="json", by_alias=True, exclude_none=True)


def dump_message_json(model: SkyfflaModel) -> str:
    return json.dumps(dump_message(model), separators=(",", ":"))
