# Skyffla Duplex `--stdio` Spec

Status: active raw byte-stream reference

## Goal

Define `skyffla ... --stdio` as a long-lived, full-duplex raw byte session suitable for true async agent-to-agent communication.

The new `--stdio` mode should:

- keep one Skyffla session alive after handshake
- allow both peers to read and write concurrently
- preserve clean payload output on `stdout`
- preserve machine-readable status and diagnostics on `stderr`
- make one-way transfer a special case of the duplex model instead of a separate mode

## Non-Goals

- turn `--stdio` into the public room automation API
- make Skyffla responsible for agent protocol semantics beyond transport and session control
- replace the room-native TUI or `machine` surfaces

## Product Definition

`--stdio` is a persistent raw byte channel.

It is intentionally separate from Skyffla's room-native multiparty model:

- use the default TUI or `machine` when you want room/member/channel semantics
- use `--stdio` when you want a raw 1:1 byte pipe

`--stdio` is not the public room automation protocol.

Each peer:

- reads local `stdin`
- writes those bytes to the remote peer
- reads remote bytes
- writes them to local `stdout`
- keeps the connection open until both directions are closed or the session is cancelled

This mode is intended to support:

- async agent collaboration
- shell pipelines that need bidirectional communication
- streaming request/response protocols
- higher-level framed agent protocols such as NDJSON
- local-network demos and agent sessions over `--local`

## Why This Design

The underlying transport already supports bidirectional streams. The current limitation is in the CLI/runtime model, not the network layer.

Relevant current code:

- transport bidirectional streams: `crates/skyffla-transport/src/lib.rs`
- duplex stdio runtime: `crates/skyffla-cli/src/runtime/stdio.rs`
- session routing between `machine` and `stdio`: `crates/skyffla-cli/src/runtime/session.rs`

Two one-way Skyffla runs can emulate turn-taking, but they are not the target architecture because they add rendezvous churn, session orchestration overhead, and awkward streaming behavior.

## High-Level Model

After the normal handshake completes, both peers enter `stdio` session mode.

The session has three active concerns:

1. machine send path
2. machine receive path
3. control path

The machine channel is long-lived and full duplex. The control channel remains responsible for handshake, cancellation, errors, and close coordination.

Discovery and connection policy are orthogonal to this session model:

- rendezvous-backed sessions should support duplex stdio
- `--local` LAN sessions should support the same duplex stdio behavior once connected

## Session Negotiation

### Handshake changes

The protocol negotiates session mode in the `Hello` message.

New field:

```rust
pub enum SessionMode {
    Machine,
    Stdio,
}
```

```rust
pub struct Hello {
    pub protocol_version: u16,
    pub session_id: String,
    pub peer_name: String,
    pub peer_fingerprint: Option<String>,
    pub capabilities: Capabilities,
    pub transport_capabilities: Vec<TransportCapability>,
    pub session_mode: SessionMode,
}
```

Rules:

- `skyffla ... --stdio` sends `session_mode = Stdio`
- `skyffla ... machine` sends `session_mode = Machine`
- the room-native TUI runs by spawning the internal `machine` backend
- peers must reject mismatched session modes during handshake
- session mode negotiation must be independent of how the peer was discovered

Rationale:

- stdio should be a session type, not a transfer kind
- negotiation should fail early and explicitly
- control messages for file/folder/clipboard remain conceptually separate from stdio machine transport

## Duplex Machine Channel

### Channel shape

Open one dedicated bidirectional machine stream after successful handshake when `session_mode = Stdio`.

Skyffla should treat this stream as an opaque byte channel.

Skyffla should not:

- impose message boundaries on payload bytes
- inspect or transform payload bytes
- assume UTF-8

Higher-level protocols such as NDJSON belong above this layer.

## Discovery and Transport Policy

The duplex stdio design must work across two connection entry points:

- rendezvous-backed peer discovery
- `--local` discovery on the same LAN

`--local` has important implications for the implementation plan:

- discovery happens over mDNS instead of rendezvous
- `join --local` may either connect to an already-advertising peer or promote itself into the advertiser/host role
- connection setup must remain direct-only
- relay paths must remain disabled
- peer endpoint addresses must remain filtered to local-network addresses
- connection policy enforcement must happen before the stdio session starts

These behaviors are transport/discovery constraints, not stdio-specific semantics.

Implication:

- do not fork duplex stdio into separate local and non-local implementations
- reuse one duplex stdio runtime after connection establishment
- keep `--local` handling confined to discovery, endpoint filtering, and connection policy checks

### Runtime shape

The stdio runtime should run three concurrent tasks:

1. `stdin -> remote machine stream`
2. `remote machine stream -> stdout`
3. `control stream handling`

Conceptually:

```rust
loop {
    select! {
        local = read_local_stdin() => write_remote_machine_stream(local),
        remote = read_remote_machine_stream() => write_local_stdout(remote),
        control = read_control_envelope() => handle_control(control),
    }
}
```

The session exits when:

- both machine directions have closed cleanly, or
- a fatal transport/protocol error occurs, or
- either peer cancels the session

## EOF and Close Semantics

EOF handling is the critical behavior change.

Rules:

- local `stdin` EOF closes only the local send half of the machine stream
- remote machine EOF closes only the local receive half
- local `stdout` remains open until remote EOF or failure
- the overall stdio session remains alive until both halves are closed

This makes the following cases work naturally:

- pure one-way send
- pure one-way receive
- request then streamed response
- interleaved async agent messages

### Required behavior

If peer A finishes writing but peer B is still streaming output back:

- peer A must continue reading from the remote machine stream
- peer A must not exit just because local stdin reached EOF

If both sides close their send halves:

- both peers drain remaining inbound bytes
- both peers exit cleanly once inbound EOF is observed

## Control Plane Responsibilities

The control stream remains open for the entire stdio session.

It is responsible for:

- negotiated handshake
- peer errors
- explicit cancellation
- explicit session close notifications if needed
- JSON event emission derived from session state

The control stream should no longer model stdio as:

- `Offer`
- `Accept`
- `Complete`

Those messages fit discrete transfers. They are the wrong abstraction for a persistent machine session.

### New control messages

Add explicit stdio session control messages if runtime coordination needs them:

```rust
pub enum ControlMessage {
    // existing variants...
    StdioClosed(StdioClosed),
    // possibly:
    // StdioError(StdioError),
}
```

```rust
pub struct StdioClosed {
    pub send_closed: bool,
}
```

This may not be strictly necessary if transport EOF is sufficient, but the spec allows it if implementation clarity benefits.

## JSON Event Model

In `--json` mode, status events remain on `stderr`.

Replace transfer-shaped stdio events with session-shaped events.

Required events:

- `machine_open`
- `machine_send_progress`
- `machine_receive_progress`
- `stdin_eof`
- `remote_eof`
- `machine_closed`
- `session_cancelled`
- `session_error`

Suggested payloads:

```json
{"event":"machine_open","session_id":"room-123"}
{"event":"machine_send_progress","bytes_done":1024}
{"event":"machine_receive_progress","bytes_done":2048}
{"event":"stdin_eof"}
{"event":"remote_eof"}
{"event":"machine_closed","send_closed":true,"receive_closed":true}
```

Rules:

- payload bytes never go to `stderr`
- JSON events never go to `stdout`
- event names should describe channel/session lifecycle, not transfer offers

## Agent Protocol Guidance

Skyffla should remain payload-agnostic, but the intended use for async agents is framed messages over the duplex byte channel.

Recommended framing for demos and skills:

- NDJSON

Example:

```json
{"type":"prompt","id":"m1","body":"Draw a tiny castle map"}
{"type":"delta","id":"m1","body":"###\n#.#\n###"}
{"type":"done","id":"m1"}
{"type":"prompt","id":"m2","body":"Annotate the rooms"}
```

Benefits:

- explicit message boundaries
- supports partial streaming
- supports interleaving multiple logical tasks
- remains shell-friendly

Skyffla itself should not require NDJSON. It should only transport bytes reliably.

## CLI Semantics

### `host` and `join`

`host` and `join` semantics remain unchanged at the rendezvous layer.

In stdio mode:

- `host <stream> --stdio` means "wait for peer, then enter duplex machine session"
- `join <stream> --stdio` means "connect to peer, then enter duplex machine session"

Even though the positional argument is still the same room / session identifier,
the resulting session is 1:1 raw byte transport, not a multiparty room API.

In local mode:

- `host <stream> --local --stdio` means "advertise on the local network, accept a direct LAN peer, then enter duplex machine session"
- `join <stream> --local --stdio` means "discover a LAN peer with mDNS or become the advertiser/host, then enter duplex machine session"
- once connected, stdio behavior should be identical to the rendezvous-backed case
- local mode should not depend on rendezvous configuration or `--server`

## Failure and Cancellation

### Fatal failures

The session fails immediately on:

- handshake mismatch
- machine stream open failure
- protocol violation
- non-recoverable transport error
- failure writing payload bytes to local stdout

### Cancellation

Either peer may cancel the stdio session over the control stream.

On cancellation:

- stop reading local stdin
- stop writing remote bytes
- stop writing to local stdout after draining no more payload
- emit a cancellation event
- exit non-zero

## Security and Trust

This redesign does not change:

- peer identity model
- trust-on-first-use behavior
- rendezvous trust assumptions

It does change the operational reality that a peer may hold a session open longer. That makes explicit cancellation and clear EOF semantics more important.

## Implementation Plan

### 1. Protocol

- add `SessionMode` to `skyffla-protocol`
- add `session_mode` to `Hello`
- reject mismatched session modes during handshake
- remove stdio’s dependency on `TransferKind::Stdio` for runtime control flow

### 2. Runtime

- replace `run_stdio_session` with a duplex session loop
- open one long-lived machine stream after handshake
- run concurrent send, receive, and control tasks
- keep the control stream open until the session fully closes
- keep the stdio runtime agnostic to whether connection establishment came from rendezvous or `--local`

### 3. Event sink

- replace transfer-shaped stdio JSON events with session-shaped events
- preserve the invariant that payload stays on `stdout` and diagnostics stay on `stderr`

### 3a. Local-mode integration

- ensure the duplex stdio session starts only after local connection policy checks pass
- keep local discovery and local-only transport filtering outside the stdio runtime
- include `discovery: "mdns"` or equivalent only in connection/waiting events, not in payload events

### 4. Tests

- rewrite current stdio end-to-end tests to the duplex contract
- add async bidirectional tests
- add EOF and cancellation tests
- add local-mode duplex stdio tests once `--local` lands on the working branch

### 5. Documentation and skill work

- update README examples once behavior is implemented
- build the Codex skill on top of the new duplex semantics
- base the demo harness on framed NDJSON messages over `--stdio`
- include a LAN-only demo path using `--local --stdio` because it removes rendezvous setup from live recordings

## Test Matrix

Required end-to-end tests:

- one-way payload still works in duplex mode
- both peers can send and receive within one session
- local stdin EOF does not terminate remote receive path
- remote EOF causes clean receive shutdown
- both peers can interleave writes before either side exits
- cancellation from either side tears down the session cleanly
- `stdout` contains only payload bytes
- `stderr` contains only status/errors/JSON events
- mismatched session modes fail during handshake
- the same duplex stdio contract holds for `--local` sessions once connected

Required local-mode tests after merge:

- `host --local --stdio` and `join --local --stdio` establish one duplex session on the LAN path
- `join --local --stdio` and `join --local --stdio` can resolve host/join roles and still enter duplex stdio
- local-only policy rejects non-direct or non-local peers before stdio session startup
- local discovery/waiting events do not pollute stdio payload on `stdout`
- `--server` configuration is irrelevant to successful local-mode stdio setup

Suggested focused tests:

- large payload streaming in both directions
- binary payload round-trip without UTF-8 assumptions
- peer exits abruptly while other side is still writing

## Open Questions

These should be resolved before implementation starts:

- whether control-plane `StdioClosed` is needed or transport EOF alone is sufficient
- whether one machine stream is enough or whether separate send/receive streams simplify shutdown
- whether progress events should be periodic or emitted on every chunk
- what exit codes should distinguish cancel, peer error, protocol error, and local I/O error

## Recommendation

Implement duplex stdio with:

- negotiated `session_mode`
- one long-lived bidirectional machine stream
- half-close semantics
- session-shaped JSON events
- one shared stdio runtime usable from both rendezvous-backed and `--local` connections
- NDJSON as the recommended framing for agent demos, but not as a Skyffla requirement

This is the cleanest path to true async agent communication without introducing an agent-specific transport mode.
