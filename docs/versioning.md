# Skyffla Versioning and Compatibility

This document defines versioning at Skyffla's three protocol boundaries:

- peer wire/control protocol
- machine protocol
- rendezvous HTTP API

These are separate contracts. They should not be inferred from the crate or binary release version.

## Compatibility Rule

Skyffla uses `major.minor` protocol versions.

- same major: compatible
- different major: incompatible
- minor versions are additive within one major

Within one major:

- new optional fields and events are allowed
- old clients may ignore additive fields and events they do not use
- behavior that requires negotiation should use explicit capabilities, not minor-version equality

## Boundary Versions

### Peer wire/control protocol

Defined in:

- `crates/skyffla-protocol/src/lib.rs`

Used in:

- peer hello / hello-ack handshake
- file-transfer capability/version advertisement in `hello`

Current constant:

- `WIRE_PROTOCOL_VERSION`

Compatibility is checked during the peer handshake. Peers must share the same wire major version.

File-transfer compatibility is negotiated separately inside the hello payload
via `FILE_TRANSFER_PROTOCOL_VERSION`. Peers may still connect on the same wire
major version while refusing `send_path` when the advertised file-transfer
major version is missing or incompatible. The current `3.x` file-transfer
major covers the native streamed path with explicit receiver credit messages and
windowed sending; `2.x` peers should be treated as incompatible for
`send_path`.

### Machine protocol

Defined in:

- `crates/skyffla-protocol/src/room.rs`

Used in:

- `room_welcome`
- wrapper-facing command/event schema

Current constant:

- `MACHINE_PROTOCOL_VERSION`

Wrappers should fail on machine major mismatch and tolerate additive minor changes.

The current machine major reflects the file-channel contract change from
blob-required metadata to explicit transfer metadata. Wrappers speaking the
older `1.x` file-channel shape should be treated as incompatible with `2.x`.

### Rendezvous HTTP API

Defined in:

- `crates/skyffla-rendezvous/src/lib.rs`

Used in:

- `/v1/rooms/...`
- `x-skyffla-rendezvous-version` response header

Current constant:

- `RENDEZVOUS_API_VERSION`

Clients should validate the rendezvous version header and require the same major API version.

## Operational Guidance

- Bump `minor` for additive, backward-compatible protocol changes within an existing family.
- Bump `major` for breaking changes to a boundary contract.
- Do not change compatibility rules ad hoc in runtime code; update this document and the relevant protocol tests together.
- Keep machine protocol examples in `docs/machine-protocol.md` aligned with the current machine version.
