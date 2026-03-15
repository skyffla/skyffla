# Self-Hosting Iroh Infrastructure

Verified against official `iroh` docs on 2026-03-08.

This note is for someone who cloned this repo and wants to run Skyffla themselves, starting small and keeping a path open to larger-scale deployment later.

## What you host

Skyffla has two separate infrastructure layers:

- Skyffla rendezvous
- `iroh` peer transport and blob transfer

In this repo today:

- You can self-host the Skyffla rendezvous service directly.
- The transport layer uses `iroh`'s default production setup unless code is changed.
- File and folder transfer use `iroh-blobs` on the same endpoint stack as chat,
  channels, and other peer traffic.

Relevant local code:

- `crates/skyffla-rendezvous/src/main.rs`
- `crates/skyffla-cli/src/main.rs`
- `crates/skyffla-transport/src/lib.rs`

## Small self-hosted setup

The simplest deployment model is:

- run your own `skyffla-rendezvous`
- keep using `iroh` public relay and discovery infrastructure

That gives you:

- your own stream registry and join/host coordination
- minimal backend surface area
- no need to run relay infrastructure on day one

This is a good fit for:

- open-source or hobby deployments
- best-effort services
- small communities
- early rollouts before usage patterns are known

Official `iroh` docs for the default public infrastructure:

- Relays overview: <https://docs.iroh.computer/concepts/relays>
- DNS discovery: <https://docs.iroh.computer/connecting/dns-discovery>

## What the default `iroh` setup implies

When Skyffla calls `iroh::Endpoint::builder()`, `iroh` uses its production defaults.

Those defaults include:

- public relays operated by Number 0
- DNS-based discovery in the default `iroh` environment

That means a small self-hosted Skyffla deployment is not fully self-contained by
default. You host rendezvous, but transport bootstrap, relay fallback, and
blob-backed file or folder transfer still depend on `iroh` public
infrastructure.

## What now uses that transport stack

Today the `iroh` endpoint is used for:

- room authority and peer links
- raw duplex `--stdio`
- blob-backed file transfer
- blob-backed folder transfer via `iroh-blobs` collections

So if you keep the default `iroh` production setup, that choice affects both the
interactive room traffic and the file or folder payload path.

## What is fine to rely on at small scale

According to the official docs:

- the public `iroh` relays are shared and free to use
- the public infrastructure may be used for production if its performance is acceptable to you

That makes the default setup reasonable for:

- small-scale public projects
- best-effort deployments
- deployments where occasional limits or degraded performance are acceptable

## What is not guaranteed

The public `iroh` infrastructure is not something to treat as a hard dependency with guarantees.

Relevant docs:

- Relays overview: <https://docs.iroh.computer/concepts/relays>
- Security & Privacy: <https://docs.iroh.computer/deployment/security-privacy>

Important caveats:

- public relays are rate-limited
- there is no uptime SLA or performance guarantee
- relay operators can observe metadata such as IPs, timing, and relayed traffic patterns
- the docs recommend not using public relays for sensitive or confidential traffic
- abuse protections may block abusive users or IPs

## Scaling path later

If usage grows, the intended next step is to move transport infrastructure off the shared public services.

Official docs:

- Dedicated infrastructure: <https://docs.iroh.computer/deployment/dedicated-infrastructure>
- Relay source code: <https://github.com/n0-computer/iroh/tree/main/iroh-relay>
- Relay releases: <https://github.com/n0-computer/iroh/releases>

The scale-out path is:

1. Keep self-hosting Skyffla rendezvous.
2. Move from `iroh` public relays to dedicated relays.
3. Configure clients to use custom relay configuration instead of `iroh`
   defaults.
4. If needed, also move discovery off the shared public environment.

In other words:

- rendezvous is already yours to host
- relays can become yours later
- discovery can also be replaced later if needed

## Recommended operating model

For most people cloning this repo today, the practical recommendation is:

1. Start by self-hosting `skyffla-rendezvous`.
2. Use the default public `iroh` infrastructure at first.
3. Watch for signs that shared relays are no longer enough:
   - reliability complaints
   - privacy requirements
   - higher traffic
   - abuse or rate-limit issues
4. Add dedicated `iroh` relay infrastructure only when those signals appear.

This keeps the initial deployment small while preserving a clear route to more
control later.
