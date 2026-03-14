# Skyffla Room / Mesh Design

Status: in progress

## Goal

Make multiparty rooms the native Skyffla abstraction.

- a room is the thing users join
- 1:1 is just a room with two members
- any member can broadcast to all or target one member
- TUI, `machine`, and language wrappers must all sit on the same room model
- wrappers must not reimplement transport or protocol logic

This is not a full network mesh in v1. It is a native multiparty product model with:

- host-owned room authority
- direct peer-to-peer member messaging
- direct peer-to-peer payload channels

One host process hosts one room.

## Current Status

Implemented so far:

- room-native protocol types in `skyffla-protocol`
- host-owned room engine in `skyffla-session`
- `machine` client surface in the CLI
- direct peer chat over peer links
- direct `machine` channel traffic over peer links
- in-band machine errors instead of process-level failures
- blob-backed file and folder channels in `machine` mode via `iroh-blobs`
- broadcast file acceptance / rejection now preserves per-recipient outcomes
- in-band `machine` stdin commands for file send / accept / reject / export without raw JSON
- rendezvous uses minimal exact room-id host lookup via `/v1/rooms/{room_id}`

Not implemented yet:

- room-native TUI on the new room engine
- a standalone wrapper-facing `machine` protocol spec
- thin Python wrapper over the `machine` API
- raw `pipe` on top of the room model

## Naming

Current `stream_id` should conceptually become `room_id`.

Use these terms consistently:

- `room_id`: the shared session namespace
- `member_id`: a participant inside a room
- `channel_id`: a logical lane inside the room
- `route`: `all` or `member(member_id)`

Keep the old `stream` term only for low-level byte streams if needed internally.

## Core Architecture

Skyffla should have one canonical room/session engine in Rust.

Everything else is an adapter on top of that.

The protocol must be treated as a first-class design artifact:

- clean
- minimal
- logically separate from runtime code
- logically separate from transport code
- documented clearly enough to be consumed by wrappers without guesswork

The protocol should not be an incidental byproduct of the CLI implementation.

### Core concepts

- `Room`
- `Member`
- `Route`
- `Channel`
- `Event`
- `Command`

### Room

Owns:

- room id
- host identity
- member table
- channel table
- routing rules
- join/leave lifecycle

### Member

Owns:

- stable room member id
- display name
- fingerprint / identity metadata
- current connection state

### Route

V1 route kinds:

- `all`
- `member(member_id)`

### Channel

A channel is a typed lane inside a room.

V1 channel kinds:

- `machine`
- `file`
- `clipboard`

Important distinction:

- `machine` channels are a Skyffla-native data path
- `file` payloads should reuse `iroh-blobs` for the data plane
- folders should be represented as blob collections rather than custom streamed archives
- `clipboard` can stay Skyffla-native unless it later benefits from the same blob path

The key rule is:

- room messages are one thing
- channel payload delivery is another thing

### Event / Command

There must be one canonical event/command model that all frontends consume:

- TUI
- machine
- Python wrapper

No frontend gets its own semantics.

This event/command model should be:

- documented as a public contract
- stable enough for wrappers to rely on
- small enough to understand without reading runtime internals
- explicit about ownership, routing, and lifecycle

## Control Plane vs Data Plane

### Control plane

Host-owned and room-native.

Responsibilities:

- join
- leave
- room welcome
- member snapshot and updates
- channel open
- channel accept / reject
- channel close
- routing and authorization
- errors

For room authority and membership:

- messages go through the host

The control plane is for setup and authority only.

It must not become a fallback chat relay path just because the host is already
connected to everyone.

The host is authoritative for:

- room membership
- member ids
- membership ordering
- channel authorization
- route validation
- room policy decisions

But ordinary room messaging should be direct between members after introduction.

That means room chat should not be carried by the host-owned authority link.
The host participates in chat as a normal room member over a peer link, not as
the control-plane relay.

For example, if `b` sends a room chat message to `c`:

- `b -> c`

If `b` broadcasts chat:

- `b -> a`
- `b -> c`
- `b -> d`

The host is included in the broadcast fanout if it is a member recipient, but
it is not a required relay hop and should not carry the chat over the authority
protocol.

So the clean split is:

- authority link: join, leave, snapshots, introductions, channel control
- peer links: chat, direct machine messages, and channel payload traffic

### Data plane

Used for channel payloads that should not live on the host relay path.

V1 rule:

- file, clipboard payload, and machine/raw payloads should be peer-to-peer when possible

But not all payload kinds should be implemented the same way.

The pragmatic split is:

- Skyffla owns the room/control plane
- Skyffla-native peer links carry chat and `machine` channel traffic
- `iroh-blobs` should carry file and folder bytes

This is important because Skyffla already has an older 1:1 custom file/folder
path, and that is not the right foundation for mesh. The room protocol should
authorize and coordinate file/folder exchange, but the actual content transfer
should delegate to `iroh-blobs` rather than reinventing verified,
content-addressed transfer again.

This means:

1. sender asks host to open a channel
2. host routes the open request to the target set
3. recipients accept or reject
4. host introduces peers as needed
5. actual payload moves directly between members

For `machine` channels, that means Skyffla peer links.

For `file` or folder channels, that means a blob transfer keyed by blob or
collection metadata, not a custom raw byte stream invented in the room runtime.

Current implementation status:

- blob-backed file and folder / collection channels are implemented in `machine` mode
- multi-recipient file accept / reject behavior now has direct coverage
- raw inline `channel_data` is intentionally invalid for file channels
- `machine` mode accepts structured in-band commands for file send/export and channel accept/reject/close, while keeping JSON as the canonical protocol format

For `route = all`, the sender fans out to one direct peer channel per accepted recipient.

This keeps:

- host logic simple
- host bandwidth under control
- per-recipient success and failure visible

## Native Multiparty Product Model

Skyffla should no longer be thought of as "1:1 plus mesh later".

Instead:

- every session is a room
- every participant is a member
- every action is routed inside the room
- 1:1 is just the smallest room

This keeps the product model coherent across:

- terminal chat
- machine integration
- wrappers
- targeted agent-to-agent channels

Chat should be treated as a routed room message, not as a channel kind.

## Room Authority and Rendezvous

Room authority should live in the host, not in rendezvous.

The host owns:

- room membership state
- member id assignment
- membership snapshots and deltas
- authoritative room ordering for join and leave
- channel authorization

Rendezvous should stay minimal.

It should help with:

- finding the host for a room id
- connecting joiners to the host

Rendezvous should work by exact room-id lookup only.

It should not:

- list rooms
- advertise rooms for browsing
- provide room discovery feeds
- expose a searchable room directory

Clients must already know the room id they want to join.

So the rendezvous contract is:

1. host registers itself as the current host for room `X`
2. joiner asks for room `X`
3. rendezvous returns the current host for room `X`, if present

This makes rendezvous a keyed host locator, not a room directory.

Current implementation status:

- the current rendezvous service already behaves mostly like this
- it stores one live host registration per exact key
- it does not expose room listing or directory APIs
- the main remaining work is contract cleanup:
  - rename `stream` terminology to `room`
  - document the room-first rendezvous contract clearly
  - keep capabilities aligned with the room-native client surfaces

Rendezvous should not own room semantics such as:

- room roster authority
- room chat routing
- channel state

This keeps the local story tight:

- `--local` and rendezvous-backed rooms use the same host-owned room engine
- only discovery changes
- room logic does not need to be duplicated across host and rendezvous services

So both discovery paths answer the same question:

- where is the host for room `X`?

Then the same room protocol and room engine take over.

## Host Model

There is one host per room.

And in v1 there should be one room per host process.

That means:

- one `host` process owns one room id
- one room id resolves to one current host
- joiners connect to that host for room authority

Different rooms can of course have different hosts.

But the implementation should not try to support one host process serving multiple rooms.

That keeps:

- room state simple
- rendezvous registration simple
- local discovery simple
- logs and tests simple
- failure handling simple

## Client Surfaces

There are three important client surfaces:

- TUI mode
- `machine` mode
- language wrappers

They must all be powered by the same room core.

### TUI mode

The TUI is a human adapter over room commands and events.

It should render:

- room id
- your member id
- member list
- room chat
- channel open/close activity
- per-peer transfer or machine-channel status

Plain text input in the TUI should default to:

- room chat broadcast

Explicit commands should exist for:

- `/members`
- `/msg <member>`
- `/pipe --to <member>`
- `/pipe --all`
- `/send --to <member>`
- `/send --all`

### `machine` mode

`machine` should be the framed machine API.

It should not be a raw opaque byte tunnel.

Why:

- a multiparty room needs source and destination metadata
- wrappers need stable commands and events
- one process stdout cannot safely carry both arbitrary bytes and room control without framing

So `machine` should expose structured room events and accept structured room commands.

Example event shapes:

```json
{"type":"room_welcome","room_id":"warehouse","self":"m1","host":"m1"}
{"type":"member_joined","member":{"id":"m2","name":"beta"}}
{"type":"chat","from":"m2","to":"all","text":"hello"}
{"type":"channel_opened","channel_id":"c7","kind":"machine","from":"m1","to":["m2"]}
{"type":"channel_data","channel_id":"c7","from":"m2","body":"..."}
{"type":"channel_closed","channel_id":"c7"}
```

Example command shapes:

```json
{"type":"send_chat","to":"all","text":"draw a comic"}
{"type":"open_channel","kind":"machine","to":["m2"]}
{"type":"send_channel_data","channel_id":"c7","body":"..."}
{"type":"close_channel","channel_id":"c7"}
```

This is the canonical machine integration surface.

### Language wrappers

Python wrappers must wrap the `machine` API.

They should not:

- speak the transport/network protocol directly
- implement host negotiation
- reimplement room state logic

They should:

- spawn `skyffla`
- talk framed commands/events over stdio
- expose ergonomic APIs
- stay thin and avoid reimplementing Rust-side semantics

For Python, use:

- Pydantic models for typed commands and events
- `uv` for project and dependency management

Example Python shape:

```python
room = skyffla.Room.join("warehouse")
room.send_chat("all", "draw a warehouse robot")
channel = room.open_machine_channel("beta")
channel.send({"type": "prompt", "body": "panel 1"})
for event in channel:
    ...
```

Wrappers do not need to know whether a routed message is:

- host-authority traffic such as join or membership
- direct member-to-member traffic such as chat or machine messages

That distinction remains inside the Rust implementation.

Internally, Skyffla should keep those link types separate as well. The
machine-facing wrapper API stays unified, but the runtime should not blur:

- authority/control messages
- peer-delivered room messages
- payload-bearing channel traffic

The Rust `machine` schema remains the source of truth.

The Python wrapper should mirror that schema with typed models, not invent a separate Python-only protocol.

## Raw Piping Still Matters

Using `machine` as the framed API does not remove Unix-style piping.

It means raw piping should become a separate surface.

Examples:

```sh
skyffla pipe <room-id> --to <member> --name report.pdf < report.pdf
skyffla pipe <room-id> --to all --name report.pdf < report.pdf
```

That keeps a clean split:

- `machine` = machine control and event API
- `pipe` = raw stdin payload into a room channel

For files and folders, there should also be a clean split:

- Skyffla `send`/`pipe` UX and room targeting stay in Skyffla
- the file/folder data plane should use `iroh-blobs`

This is cleaner than overloading one top-level mode for both jobs.

## CLI Shape

Because rooms are the native abstraction, the CLI should not introduce a redundant `room` noun.

Prefer:

```sh
skyffla host <room-id>
skyffla join <room-id>
skyffla host <room-id> machine
skyffla join <room-id> machine
skyffla pipe <room-id> --to <member>
skyffla send <room-id> <path> --to <member>
```

Not:

```sh
skyffla room host <room-id>
skyffla room join <room-id>
```

If rooms are the core model, then:

- `host` means host a room
- `join` means join a room
- 1:1 is just a two-member room

So the extra `room` command only adds noise.

## Canonical Rust Layers

### `skyffla-protocol`

Owns the canonical serialized schema.

It should define:

- room commands
- room events
- member and route types
- channel lifecycle types

It should become the source for machine schemas used by wrappers.

It should be kept logically separate from:

- transport implementation details
- CLI/TUI behavior
- runtime orchestration code

Protocol types should describe the model and wire contract, not the local implementation strategy.

The protocol crate should be well documented.

At minimum, it should clearly document:

- room lifecycle
- member lifecycle
- route semantics
- channel lifecycle
- command and event meanings
- which actions are host-authoritative vs direct member-to-member
- how wrappers are expected to consume the machine API

### Room/session engine

Owns runtime semantics:

- membership
- routing
- channel state
- delivery rules
- host-side coordination

This should be shared by TUI and `machine`.
This should be shared by TUI and `machine`.

### Transport layer

Owns:

- peer connections
- host/member authority links
- direct member-to-member messaging links
- direct per-channel payload links
- low-level streams

It should not own user-facing room semantics.

### Adapters

- TUI adapter
- machine adapter
- future language wrapper processes built on stdio

## Protocol Direction

The current 1:1-oriented message model should be reshaped around rooms and channels.

Instead of centering the protocol around transfer primitives like:

- `Offer`
- `Accept`
- `Reject`
- `Complete`

the new protocol should center on:

- room lifecycle
- member lifecycle
- chat delivery
- channel lifecycle
- routed payload events

Reasonable command/event families:

- `JoinRoom`
- `RoomWelcome`
- `MemberSnapshot`
- `MemberJoined`
- `MemberLeft`
- `SendChat`
- `ChatDelivered`
- `OpenChannel`
- `ChannelOpened`
- `ChannelAccepted`
- `ChannelRejected`
- `ChannelData`
- `ChannelClosed`
- `Error`

Channel kinds may still internally use offer/accept style behavior, but that should be an implementation detail of channel lifecycle, not the top-level product model.

The important routing rule is:

- join and membership authority are host-owned
- ordinary room messages are direct peer-to-peer
- broadcast is sender fanout to all current members in the roster

## What Should Be Removed or Reworked

### Rework `SessionMode`

`SessionMode` currently pushes the implementation toward separate runtime worlds.

In the room-native design:

- room semantics are primary
- TUI and machine are adapters
- channel kind carries payload semantics

If a mode concept remains, it should describe the client surface, not the room model.

### Rework the 1:1-centric protocol taxonomy

Current protocol types are too shaped around:

- one peer
- one transfer
- one control lane

They should be replaced or refactored into room-and-channel-native types.

### Remove the idea that raw stdio is the core public machine API

Raw duplex bytes are still useful, but they should be represented as:

- a channel kind
- or a `pipe` command

They should not define the overall automation surface.

### Remove protocol duplication in wrappers

Wrappers should not mirror Rust session logic.

They should consume the official machine API.

## Logical Build Plan

The build should happen in stages, but each stage should move toward the final native design, not preserve old 1:1 abstractions.

### Phase 1: Canonical protocol reshape

Status: done

Goals:

- introduce room/member/channel terminology in `skyffla-protocol`
- define room command and event schema
- define route and member identity types
- define channel lifecycle types
- write clear protocol docs alongside the types

Output:

- canonical Rust protocol types for room-native operation
- protocol documentation that can serve as the wrapper contract
- serialization tests for all command/event families

Tests:

- encode/decode round trips for room commands and events
- invalid route or member-target validation
- schema consistency tests for channel lifecycle types

### Phase 2: Core room engine in Rust

Status: done

Goals:

- implement host-side room state
- implement member join/leave and snapshots
- implement direct member routing based on host-owned membership
- implement direct broadcast fanout based on current member snapshots
- unify runtime logic beneath TUI and stdio

Output:

- room engine with host-managed membership and direct routing

Tests:

- unit tests for room state transitions
- host assigns stable member ids
- member join/leave updates are broadcast correctly
- broadcast chat fans out directly to all current members except sender
- direct chat reaches only the target member
- membership changes invalidate stale broadcast targets correctly
- one host process maps to exactly one room

### Phase 3: Machine adapter

Status: done

Goals:

- expose room commands/events through `machine`
- remove reliance on raw opaque stdio as the main automation surface
- support machine channels and channel data events

Output:

- canonical machine API for wrappers

Tests:

- end-to-end room join through stdio
- machine client receives member updates
- machine client broadcasts chat via direct fanout
- machine client targets a single member
- machine client opens and closes a machine channel

### Phase 4: Direct payload channels

Status: mostly done for `machine`; file path is in progress

Goals:

- implement direct member-to-member channel establishment after room coordination
- support channel kinds such as `machine`, `file`, and `clipboard`
- use one direct peer channel per accepted recipient for `route = all`

Output:

- peer-to-peer `machine` payload path under room control
- room-coordinated channel lifecycle that can later back blob transfers cleanly

Tests:

- host-routed open, peer-accepted direct channel establishment
- sender to one recipient
- sender to all recipients with per-recipient success/failure
- failure isolation for one rejected or disconnected recipient
- direct chat path and direct payload path coexist without host relay confusion

Non-goals for this phase:

- do not build a new custom mesh file/folder byte-stream implementation
- do not port the old 1:1 file/folder transfer path into rooms

### Phase 5: Blob-backed files and folders

Status: in progress

Goals:

- integrate `iroh-blobs` for file and folder payload transfer
- keep Skyffla room UX/control semantics around offer, accept, reject, and targeting
- represent file/folder payloads as room-authorized channels that resolve to blob metadata
- support folders via blob collections rather than custom tar-stream logic in the mesh runtime

Output:

- Skyffla-controlled file/folder UX with `iroh-blobs` as the data plane
- clear separation between room authorization and blob transfer

Implemented so far:

- single-file and folder / collection blob-backed channels in `machine` mode
- local file import, remote fetch, and local export on top of the shared transport endpoint
- local directory import, remote fetch, and local directory export via blob collections
- file-channel validation that rejects inline `channel_data`
- per-recipient reject / accept coverage for broadcast file channels

Remaining work:

- file UX outside `machine`

Tests:

- offer file to one member, accept, transfer by blob metadata
- offer file to all, per-recipient acceptance and independent success/failure
- offer folder/collection and verify collection metadata round-trips
- rejecting one recipient does not affect the others
- resuming or reusing already-present content works when supported by `iroh-blobs`

### Phase 6: TUI adapter on top of room engine

Status: not started

Goals:

- port interactive mode to room-native state
- support room member UI and routing commands

Output:

- human-facing room client using the same core as stdio

Tests:

- command parsing for member targeting
- integration tests for broadcast and direct chat
- room UI state updates on join/leave

### Phase 7: `pipe`

Status: not started

Goals:

- restore Unix-style raw stdin payload support as a separate surface
- map raw stdin to a room channel
- keep this focused on `machine`/raw payloads, not as a replacement for blob-backed file transfer

Output:

- clean raw payload CLI independent from `machine`

Tests:

- `pipe --to <member>`
- `pipe --to all`
- buffered stdin replay for multi-recipient fanout if required

### Phase 8: Wrapper-facing protocol docs

Status: not started

Goals:

- write a clean standalone `machine` protocol spec for wrapper authors
- document command and event shapes independently of runtime internals
- document host-authority vs peer-delivered behavior at the contract level

Output:

- a wrapper-facing machine protocol document that matches the Rust schema

Tests:

- example command/event payloads stay in sync with protocol types

### Phase 9: Python wrapper

Status: not started

Goals:

- wrap the machine API only
- provide typed command/event APIs
- provide channel helpers
- use Pydantic models for commands and events
- use `uv` for project management

Output:

- thin wrappers with no protocol reimplementation

Tests:

- wrapper can join a room
- wrapper can send room chat
- wrapper can open a machine channel
- wrapper receives typed events matching the Rust schema

## Remaining Execution Order

From the current state, the recommended order is:

1. finish the remaining file UX work outside raw `machine` command entry
2. port the TUI onto the room engine
3. document the wrapper-facing `machine` contract cleanly
4. build the thin Python wrapper on top of that contract
5. add raw `pipe` as a separate surface after the wrapper-facing contract is stable

## Test Strategy

The test pyramid should be explicit from the start.

### Protocol tests

In `skyffla-protocol`:

- serialization round trips
- envelope validation
- route and member validation
- documentation-level examples that round-trip against real protocol types

### Engine tests

In the room engine:

- deterministic state-machine tests
- routing correctness
- channel lifecycle correctness

### CLI adapter tests

In `skyffla-cli`:

- machine adapter end-to-end tests
- room host/join integration tests
- TUI command parsing tests

### Wrapper contract tests

For Python:

- smoke tests against the machine API
- fixture-driven event/command decoding tests

### Scenario tests

End-to-end scenarios that prove user-facing behavior:

- two-member room as 1:1 special case
- three-member room broadcast chat
- targeted direct message
- machine channel open to one member
- machine channel fanout to all
- exact room-id rendezvous lookup without room listing

## Immediate First Work

Start here:

1. redefine the protocol around `room`, `member`, `route`, and `channel`
2. build a small in-memory room engine with join/leave and routing
3. add end-to-end tests for:
   - host plus two members
   - member snapshot
   - broadcast chat
   - direct chat

Do not start with wrappers.
Do not start with TUI polish.
Do not start with raw piping.

The first milestone is:

- a room-native Rust protocol
- a room-native Rust runtime
- broadcast and 1:1 routing under one model

That milestone is complete. The current frontier is the remaining file UX work,
then TUI, then wrappers.

## Summary

The clean design is:

- room-first, not 1:1-first
- host-owned room authority
- minimal rendezvous used only for discovery and host lookup
- rendezvous exact-match lookup only, no room directory
- one host process per room
- direct member-to-member messaging
- direct sender fanout for broadcast
- one canonical Rust room protocol
- protocol kept clean, logically separate, and well documented
- one room/session engine
- TUI and `machine` as adapters
- wrappers on top of the `machine` API
- raw piping preserved as a separate `pipe` surface

That gives:

- a native multiparty mental model
- 1:1 as a special case
- no duplicated protocol logic across languages
- a clean path to wrappers and richer agent scenarios later
