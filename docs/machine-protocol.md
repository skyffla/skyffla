# Skyffla Machine Protocol

This document defines the wrapper-facing `machine` contract for Skyffla rooms.

The source of truth is the Rust schema in:

- `crates/skyffla-protocol/src/room.rs`

The examples below are kept in sync with serialization tests in that same file.

## Transport

`machine` is a framed JSON protocol over stdio.

- commands: newline-delimited JSON objects written to `stdin`
- events: newline-delimited JSON objects emitted on `stdout`
- runtime/status logs: human or JSON logs on `stderr`

Wrappers should treat:

- `stdin` as the command stream
- `stdout` as the canonical event stream
- `stderr` as diagnostics only

The convenience slash commands accepted by the CLI in `machine` mode are not part of this protocol contract.

## Versioning

Current protocol version:

- `1`

The version is emitted in the initial `room_welcome` event.

## Identifiers

- `room_id`: shared room namespace
- `member_id`: stable participant identity inside one room
- `channel_id`: logical lane for non-chat payloads

Identifiers are opaque strings. Wrappers should not infer semantics from them.

## Routes

Routes are explicit JSON values:

Broadcast:

```json
{"type":"all"}
```

Direct:

```json
{"type":"member","member_id":"m2"}
```

V1 routes are only:

- `all`
- `member`

## Delivery Model

There are two logical categories of events:

Host-authoritative events:

- `room_welcome`
- `member_snapshot`
- `member_joined`
- `member_left`
- channel accept / reject / close authority decisions

Peer-delivered events:

- `chat`
- `channel_data`
- file readiness/export completion once the channel is established

Wrappers should not need to care which transport link carried an event, but they should understand that:

- room membership and channel control are authoritative
- chat and channel payload traffic are end-to-end member traffic

## Commands

### `send_chat`

Broadcast:

```json
{"type":"send_chat","to":{"type":"all"},"text":"hello room"}
```

Direct:

```json
{"type":"send_chat","to":{"type":"member","member_id":"m2"},"text":"hello beta"}
```

### `open_channel`

Open a machine channel:

```json
{
  "type":"open_channel",
  "channel_id":"c7",
  "kind":"machine",
  "to":{"type":"member","member_id":"m2"},
  "name":"agent-link"
}
```

Open a file channel:

```json
{
  "type":"open_channel",
  "channel_id":"f1",
  "kind":"file",
  "to":{"type":"member","member_id":"m2"},
  "name":"report.pdf",
  "size":1234,
  "mime":"application/pdf",
  "blob":{"hash":"abc123","format":"blob"}
}
```

Rules:

- `file` channels require `blob`
- `machine` and `clipboard` channels must not include `blob`

### `send_file`

`send_file` is a local convenience command for the sender. It imports a local file or directory into the blob store, then lowers to `open_channel(kind=file, blob=...)`.

```json
{
  "type":"send_file",
  "channel_id":"f1",
  "to":{"type":"member","member_id":"m2"},
  "path":"./report.txt",
  "name":"report.txt",
  "mime":"text/plain"
}
```

### `accept_channel`

```json
{"type":"accept_channel","channel_id":"c7"}
```

### `reject_channel`

```json
{"type":"reject_channel","channel_id":"c7","reason":"busy"}
```

### `send_channel_data`

For `machine` and `clipboard` channels:

```json
{"type":"send_channel_data","channel_id":"c7","body":"partial output"}
```

`file` channels do not use inline `channel_data`.

### `close_channel`

```json
{"type":"close_channel","channel_id":"c7","reason":"done"}
```

### `export_channel_file`

`export_channel_file` is a local convenience command for recipients after a file channel becomes ready.

```json
{"type":"export_channel_file","channel_id":"f1","path":"./downloads/report.txt"}
```

## Events

### `room_welcome`

```json
{
  "type":"room_welcome",
  "protocol_version":1,
  "room_id":"warehouse",
  "self_member":"m1",
  "host_member":"m1"
}
```

### `member_snapshot`

```json
{
  "type":"member_snapshot",
  "members":[
    {"member_id":"m1","name":"alpha","fingerprint":"fp-m1"},
    {"member_id":"m2","name":"beta","fingerprint":"fp-m2"}
  ]
}
```

### `member_joined`

```json
{
  "type":"member_joined",
  "member":{"member_id":"m3","name":"gamma","fingerprint":"fp-m3"}
}
```

### `member_left`

```json
{"type":"member_left","member_id":"m3","reason":"disconnected"}
```

### `chat`

```json
{
  "type":"chat",
  "from":"m2",
  "from_name":"beta",
  "to":{"type":"all"},
  "text":"hello"
}
```

### `channel_opened`

```json
{
  "type":"channel_opened",
  "channel_id":"c7",
  "kind":"machine",
  "from":"m2",
  "from_name":"beta",
  "to":{"type":"member","member_id":"m1"},
  "name":"agent-link",
  "size":null,
  "mime":null,
  "blob":null
}
```

### `channel_accepted`

```json
{"type":"channel_accepted","channel_id":"c7","member_id":"m1","member_name":"alpha"}
```

### `channel_rejected`

```json
{"type":"channel_rejected","channel_id":"c7","member_id":"m1","member_name":"alpha","reason":"busy"}
```

### `channel_data`

```json
{
  "type":"channel_data",
  "channel_id":"c7",
  "from":"m2",
  "from_name":"beta",
  "body":"partial output"
}
```

### `channel_closed`

```json
{"type":"channel_closed","channel_id":"c7","member_id":"m1","member_name":"alpha","reason":"done"}
```

### `channel_file_ready`

```json
{
  "type":"channel_file_ready",
  "channel_id":"f1",
  "blob":{"hash":"abc123","format":"blob"}
}
```

For folders, `format` is `collection`.

### `channel_file_exported`

```json
{"type":"channel_file_exported","channel_id":"f1","path":"./downloads/report.txt","size":1234}
```

### `error`

```json
{"type":"error","code":"command_failed","message":"channel c7 is closed","channel_id":"c7"}
```

## Validation Rules

- `send_chat.text` must not be empty
- `send_channel_data.body` must not be empty
- `member_snapshot.members` must not be empty
- `file` channels require blob metadata
- non-file channels must not include blob metadata
- `send_file` and `export_channel_file` are local machine commands, not peer-forwarded room commands

## Wrapper Guidance

Wrappers should:

- parse every stdout line as a `MachineEvent`
- serialize every stdin command as a `MachineCommand`
- treat unknown fields conservatively
- route by `member_id` and `channel_id`, not display names
- use `stderr` only for diagnostics and debugging

Wrappers should not:

- reimplement room authority logic
- infer delivery paths from event types
- parse slash commands
