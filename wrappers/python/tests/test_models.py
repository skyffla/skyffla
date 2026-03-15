from skyffla import (
    ChannelKind,
    Chat,
    MACHINE_PROTOCOL_VERSION,
    OpenChannel,
    ProtocolVersion,
    RouteAll,
    RouteMember,
    RoomWelcome,
    __version__,
    dump_message,
    ensure_machine_protocol_version,
    normalize_route,
    parse_cli_version,
    parse_machine_command,
    parse_machine_event,
)
from skyffla.client import SkyfflaMachineProtocolMismatch


def test_documented_chat_event_shape_round_trips() -> None:
    event = parse_machine_event(
        {
            "type": "chat",
            "from": "m2",
            "from_name": "beta",
            "to": {"type": "all"},
            "text": "hello",
        }
    )

    assert isinstance(event, Chat)
    assert event.from_ == "m2"
    assert event.from_name == "beta"
    assert isinstance(event.to, RouteAll)
    assert event.text == "hello"


def test_room_welcome_uses_structured_protocol_version() -> None:
    event = parse_machine_event(
        {
            "type": "room_welcome",
            "protocol_version": {"major": 1, "minor": 0},
            "room_id": "warehouse",
            "self_member": "m1",
            "host_member": "m1",
        }
    )

    assert isinstance(event, RoomWelcome)
    assert event.protocol_version == MACHINE_PROTOCOL_VERSION
    assert dump_message(event) == {
        "type": "room_welcome",
        "protocol_version": {"major": 1, "minor": 0},
        "room_id": "warehouse",
        "self_member": "m1",
        "host_member": "m1",
    }


def test_documented_open_channel_command_shape_round_trips() -> None:
    command = parse_machine_command(
        {
            "type": "open_channel",
            "channel_id": "c7",
            "kind": "machine",
            "to": {"type": "member", "member_id": "m2"},
            "name": "agent-link",
        }
    )

    assert isinstance(command, OpenChannel)
    assert command.channel_id == "c7"
    assert command.kind == ChannelKind.MACHINE
    assert isinstance(command.to, RouteMember)
    assert command.to.member_id == "m2"
    assert dump_message(command) == {
        "type": "open_channel",
        "channel_id": "c7",
        "kind": "machine",
        "to": {"type": "member", "member_id": "m2"},
        "name": "agent-link",
    }


def test_normalize_route_accepts_all_and_member_ids() -> None:
    assert isinstance(normalize_route("all"), RouteAll)
    route = normalize_route("m9")
    assert isinstance(route, RouteMember)
    assert route.member_id == "m9"


def test_parse_cli_version_reads_standard_output() -> None:
    assert parse_cli_version("skyffla 0.2.0") == "0.2.0"
    assert parse_cli_version(f"skyffla {__version__}") == __version__


def test_machine_protocol_version_requires_matching_major() -> None:
    ensure_machine_protocol_version(ProtocolVersion(major=1, minor=7))

    try:
        ensure_machine_protocol_version(ProtocolVersion(major=2, minor=0))
    except SkyfflaMachineProtocolMismatch as exc:
        assert "same machine protocol major version" in str(exc)
    else:
        raise AssertionError("expected a machine protocol mismatch")
