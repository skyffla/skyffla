# Skyffla Machine Protocol

This document defines the wrapper-facing `machine` contract for Skyffla's
multi-party room model.

`machine` is the room-native automation surface. It is the API wrappers should
use when they want to participate in Skyffla rooms rather than raw 1:1 byte
streams.

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

- `2.1`

The version is emitted in the initial `room_welcome` event.

Compatibility rule:

- same major: compatible
- different major: incompatible
- minor versions are additive within one major

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
- file transfer progress and receive completion once the channel is established

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
  "transfer":{
    "item_kind":"file",
    "integrity":{"algorithm":"blake3","value":"feedbeef"}
  }
}
```

Rules:

- `file` channels require `transfer`
- `machine` and `clipboard` channels must not include `transfer`

### `send_path`

`send_path` is the primary machine command for sending a file or folder.

Current behavior:

- regular files: open a provisional `file` channel early with
  `transfer.item_kind = "file"` and `transfer.integrity = null`, allow the
  receiver to accept or reject immediately, then publish final transfer
  metadata with `update_channel_transfer` once preparation completes
- directories: prepare a directory manifest, open a `file` channel with
  `transfer.item_kind = "folder"` and a whole-transfer digest, then stream
  files over the native transfer path

Accepted transfers are saved automatically into the receiver's resolved
download directory. There is no separate export step in the normal flow.

```json
{
  "type":"send_path",
  "channel_id":"f1",
  "to":{"type":"member","member_id":"m2"},
  "path":"./report.txt",
  "name":"report.txt",
  "mime":"text/plain"
}
```

### `update_channel_transfer`

`update_channel_transfer` finalizes a previously opened provisional file
channel once the sender has finished preparing the transfer metadata.

```json
{
  "type":"update_channel_transfer",
  "channel_id":"f1",
  "size":1234,
  "transfer":{
    "item_kind":"file",
    "integrity":{"algorithm":"blake3","value":"feedbeef"}
  }
}
```

### `accept_channel`

```json
{"type":"accept_channel","channel_id":"c7"}
```

For provisional file channels, `accept_channel` may arrive before the sender
has finished preparing the final integrity metadata. Single-file transfers may
start downloading immediately after accept, while folder transfers stay
accepted but waiting until a matching `channel_transfer_ready` arrives.

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

Recipients do not need a separate export command in the normal flow. Accepted
file and folder transfers are saved automatically into the local download
directory.

## Events

### `room_welcome`

```json
{
  "type":"room_welcome",
  "protocol_version":{"major":2,"minor":2},
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
  "mime":null
}
```

For file and folder channels, `transfer.integrity` may be absent in the
initial `channel_opened` event while the sender is still preparing or
finalizing the transfer metadata.

### `channel_accepted`

```json
{"type":"channel_accepted","channel_id":"c7","member_id":"m1","member_name":"alpha"}
```

### `channel_transfer_ready`

```json
{
  "type":"channel_transfer_ready",
  "channel_id":"f1",
  "size":1234,
  "transfer":{
    "item_kind":"file",
    "integrity":{"algorithm":"blake3","value":"feedbeef"}
  }
}
```

This event updates a previously opened provisional file channel with the final
transfer metadata for a prepared folder transfer. A receiver that already
accepted the folder should begin the native receive path once this event
arrives.

### `channel_transfer_finalized`

```json
{
  "type":"channel_transfer_finalized",
  "channel_id":"f1",
  "size":1234,
  "transfer":{
    "item_kind":"file",
    "integrity":{"algorithm":"blake3","value":"feedbeef"}
  }
}
```

This event publishes the final whole-file digest for a single-file transfer.
Receivers save into a temporary path first and only emit `channel_path_received`
after the finalized digest matches the locally downloaded bytes.

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

### `channel_path_received`

```json
{
  "type":"channel_path_received",
  "channel_id":"f1",
  "path":"./downloads/report.txt",
  "size":1234
}
```

For folders, `path` is the saved directory root.

This event is the normal receive completion signal for accepted transfers.

### `error`

```json
{"type":"error","code":"command_failed","message":"channel c7 is closed","channel_id":"c7"}
```

## Validation Rules

- `send_chat.text` must not be empty
- `send_channel_data.body` must not be empty
- `member_snapshot.members` must not be empty
- `file` channels require transfer metadata
- provisional file channels may omit `transfer.integrity` in the initial
  `channel_opened`, but the sender must later publish final metadata with
  `update_channel_transfer` before bytes move
- non-file channels must not include transfer metadata
- `send_path` is a local machine command, not a peer-forwarded room command
- accepted file and folder transfers save automatically; wrappers do not need a
  follow-up export command in the normal flow

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
