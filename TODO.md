# Skyffla Room / Mesh TODO

This file tracks the remaining work in the order we should tackle it.

## Current Priority Order

1. Port the TUI onto the room engine
2. Write the wrapper-facing `machine` protocol spec
3. Build the thin Python wrapper
4. Add `pipe` as a separate raw payload surface

## 1. Rendezvous Cleanup

- [x] Rename rendezvous API and docs conceptually from `stream` to `room`
- [x] Keep exact room-id lookup semantics explicit
- [x] Verify host registration remains one live host per room id
- [x] Keep rendezvous free of room roster, chat, or channel semantics
- [x] Update CLI/client naming where appropriate without widening rendezvous responsibilities

Exit criteria:

- rendezvous is clearly documented and tested as a minimal room host locator
- naming aligns with the room-first architecture
## 2. File / Folder Work

- [x] Blob-backed single-file channels in `machine`
- [x] Add folder / collection channels on top of `iroh-blobs`
- [x] Add end-to-end tests for folder round-trips
- [x] Add multi-recipient file fanout coverage
- [x] Add rejection / partial-failure coverage for multi-recipient file sends
- [x] Decide and implement the room-native CLI surface for file send/export outside raw JSON entry
- [x] Keep file-channel UX in-band and machine-friendly

Exit criteria:

- one member can send a folder to one recipient
- one member can send a file to multiple recipients with independent success/failure
- file and folder channels do not use inline `channel_data`

## 3. TUI on Room Engine

- [ ] Replace remaining 1:1 assumptions in the interactive runtime
- [ ] Show room id, self member id, and host member id
- [ ] Render live member roster updates
- [ ] Default text entry to room broadcast chat
- [ ] Add direct-message command for targeting one member
- [ ] Show channel open / accept / reject / close events
- [ ] Add file accept / reject / export flows in the TUI
- [ ] Add integration coverage for room join, broadcast chat, and direct chat through the TUI-facing runtime

Exit criteria:

- a three-member room is usable from the TUI
- direct chat and broadcast chat both work
- file offers can be accepted and exported from the TUI

## 4. Wrapper-Facing Machine Spec

- [ ] Write a standalone `machine` protocol document
- [ ] Document all command and event shapes
- [ ] Document route semantics and member/channel identifiers
- [ ] Document which operations are host-authoritative vs peer-delivered
- [ ] Add example payloads that are kept in sync with protocol tests

Exit criteria:

- a wrapper author can implement against the spec without reading runtime internals

## 5. Python Wrapper

- [ ] Create a `uv`-managed Python project
- [ ] Add thin process management for `skyffla ... machine`
- [ ] Mirror the Rust schema with Pydantic command/event models
- [ ] Add a small sync or async room client API
- [ ] Add smoke tests against a real local `machine` session
- [ ] Add one minimal example covering room chat and one machine channel

Exit criteria:

- the wrapper is thin
- the wrapper does not reimplement room or transport semantics
- typed Python events match the Rust `machine` schema

## 6. Raw `pipe`

- [ ] Define the final `pipe` CLI surface
- [ ] Map stdin payloads onto room-native channels
- [ ] Support `--to <member>` and broadcast fanout
- [ ] Add end-to-end tests for one-recipient and multi-recipient piping

Exit criteria:

- raw stdin payloads are available as a separate surface
- `pipe` stays distinct from the framed `machine` API

## Ongoing Cleanup

- [ ] Keep `skyffla-protocol` as the canonical documented contract
- [ ] Keep `machine` runtime helpers small and unit-testable
- [ ] Continue separating authority-link and peer-link responsibilities in code
- [ ] Remove obsolete 1:1-only code paths when the room-native replacements are complete
