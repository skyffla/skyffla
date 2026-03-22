# File Transfer Architecture

This document describes the current Skyffla file and folder transfer
architecture.

It is the implementation-oriented companion to:

- `docs/machine-protocol.md`
- `docs/versioning.md`
- `docs/file-transfer-benchmarks.md`

## Goals

The current design optimizes for:

- simple `send_path` user flow
- direct native disk-to-disk transfer
- bounded memory usage
- verified writes before rename
- explicit incompatibility handling between peers

## Layers

Skyffla file transfer has three layers.

### Peer Handshake

The normal peer handshake advertises a dedicated file-transfer protocol
version.

This is used to:

- detect incompatible peers before transfer starts
- evolve file-transfer semantics separately from the base wire protocol

Primary sources:

- `crates/skyffla-protocol/src/lib.rs`
- `crates/skyffla-cli/src/runtime/handshake.rs`

### Room / Machine Control Plane

The room and machine protocol authorizes and coordinates transfers.

This layer handles:

- `send_path`
- provisional `channel_opened`
- accept / reject
- `channel_transfer_ready` for folders
- `channel_transfer_finalized` for files
- transfer progress events
- `channel_path_received`

Primary sources:

- `docs/machine-protocol.md`
- `crates/skyffla-protocol/src/room.rs`
- `crates/skyffla-session/src/room.rs`

### Native Transfer Data Plane

After a transfer is accepted, payload bytes move over Skyffla's dedicated iroh
transfer ALPN.

Skyffla relies on iroh / QUIC for:

- peer connectivity
- direct vs relay path selection
- encrypted authenticated transport
- retransmission
- congestion control
- pacing
- stream multiplexing

Skyffla-specific transfer logic stays focused on:

- authorization
- save-path semantics
- integrity verification
- scheduling
- bounded application memory

Primary source:

- `crates/skyffla-transport/src/lib.rs`

## Receiver Write Semantics

The receiver resolves one destination root locally from:

- an explicit CLI destination option, if present
- otherwise a persisted download location, if configured
- otherwise the current working directory

Writes then follow this pattern:

- write into `.skyffla.part` temp paths
- verify received bytes
- rename only after verification succeeds

For folders, the receiver creates a temp root first and renames the verified
root into place at the end.

## Single-File Flow

Single-file transfer uses overlap between hashing and sending.

1. sender opens a provisional file channel early
2. receiver may accept before sender digest finalization finishes
3. sender registers the outgoing file immediately by path plus size
4. receiver connects to the transfer ALPN
5. sender advertises file size
6. receiver grants an initial credit window
7. sender streams bytes while computing the whole-file BLAKE3 digest inline
8. receiver writes to a `.skyffla.part` file and computes the same digest
   inline
9. sender publishes `channel_transfer_finalized` with the final digest
10. receiver renames into place only if local size and local digest match

This removes the old full-file pre-hash stall without introducing Merkle trees
or chunk proofs.

## Folder Flow

Folder transfer uses a lightweight plan plus bounded workers.

1. sender walks the tree and builds a lightweight folder plan
2. the plan includes normalized relative paths, file sizes, and empty-directory
   markers
3. sender opens a provisional folder channel
4. receiver may accept as soon as the plan is ready
5. sender publishes `channel_transfer_ready`
6. receiver opens one shared native transfer connection for the folder
7. sender replies with the directory manifest on that connection
8. receiver starts a bounded worker pool
9. each worker opens a per-file request stream plus credit stream over the
   shared connection
10. sender serves each file stream while computing that file's BLAKE3 hash
    inline
11. sender sends a trailing per-file finalization hash
12. receiver hashes each file while receiving and verifies the finalization
    hash
13. receiver renames the folder temp root into place only after the full
    planned set completes successfully

Important properties:

- folder transfer no longer blocks on full upfront hashing
- folder workers share one transfer connection
- worker count is tunable via `SKYFFLA_DIRECTORY_TRANSFER_WORKERS`
- the current default is `4`
- empty directories are preserved

## Integrity Model

Files:

- sender may start streaming before final digest publication
- receiver hashes while receiving
- rename happens only after `channel_transfer_finalized` matches local bytes

Folders:

- each planned file is verified independently during transfer
- success is defined by completing and verifying the full planned set
- the current folder design does not require an upfront whole-transfer digest

## Current Performance Shape

Measured local results are tracked in `docs/file-transfer-benchmarks.md`.

Current broad takeaways:

- single-file overlap removed the largest end-to-end latency penalty
- the first folder-overlap version regressed because it opened a fresh
  transfer connection per file
- reusing one shared folder transfer connection recovered that throughput loss
- `N=4` is currently the best measured folder worker default on this machine
- many-small-files workloads are slower than fewer-medium-files workloads,
  which indicates visible per-file overhead

## Current Non-Goals

The current design intentionally does not include:

- `iroh-blobs` in the file-transfer hot path
- Merkle trees or per-range proofs
- resume-oriented chunk verification
- sophisticated application-level congestion control beyond the simple
  receiver-credit layer

If future work is needed, the most likely next optimization area is reducing
many-small-files overhead rather than changing the large-file transport path.
