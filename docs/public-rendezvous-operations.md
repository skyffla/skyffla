# Public Rendezvous Operations

The public service is a small discovery registry. It does not relay peer file,
folder, chat or pipe payloads. Those use iroh directly or iroh's public relays.
Do not add autoscaling, a metered load balancer or a paid relay without reviewing
the cost controls separately.

## Application limits and ownership

Rendezvous API `3.0` requires a private, randomly generated 256-bit bearer token
for registration and deletion. Only its hash is stored. Public lookup data must
never serve as an ownership credential. See [versioning.md](versioning.md) for
the migration and client compatibility rules.

The app limits request bodies to 4 KiB, tickets to 2 KiB, room IDs to 64 ASCII
characters, tickets to 16 addresses, and the registry to 1000 live rooms. The
maximum TTL is one hour. SQLite growth is limited to 16384 pages (64 MiB with
the default 4 KiB pages); expired rows are indexed and reused. Request limits
run before JSON parsing, including invalid paths and bodies. All clients share
600 requests per configured rate window, in addition to each IP's allowance.
Rate limiter state is bounded and idle entries expire.

Bind the application to loopback. Terminate TLS in nginx and overwrite
`X-Forwarded-For` with `$remote_addr`. Then enable
`SKYFFLA_RENDEZVOUS_TRUST_PROXY_HEADERS=true`; the app trusts this header only
from loopback connections. Never expose the application port directly.

Nginx should additionally enforce per-IP and global request/connection limits,
4 KiB bodies, short header/body/upstream timeouts, and TLS 1.2 or newer. Serve
ACME HTTP challenges on port 80, but do not proxy discovery over plain HTTP.
Legacy HTTP clients must receive an upgrade-required response, not a redirect
that might forward ownership credentials. Renew certificates automatically and
reload nginx after renewal.

## Enforced network cost limit

On a **dedicated** Linux VM with nftables and systemd installed:

```sh
sudo bash scripts/ops/install-cost-guard.sh
sudo nft list table inet skyffla_cost_guard
systemctl is-enabled skyffla-cost-guard.service
```

This installs a kernel output limiter of **8 KiB/second across public TCP
responses from ports 80, 443 and 8080**, with a 128 KiB burst. The rule covers
IPv4 and IPv6, TLS handshakes, rejected requests and retransmissions. Excess
packets are dropped, so deliberate floods can make discovery slow or unavailable.
SSH and unrelated traffic are outside this limit. The rule loads before
network startup, and nginx requires the guard service to start successfully.
Stopping the guard does not remove its kernel rules. Do not flush nftables or
replace this table without restoring the protection.

At that rate, a continuously saturated 31-day month is about 20.5 GiB of counted
public-service output, plus packet segmentation overhead and the small startup burst. Allow
headroom when estimating billing. At the published Premium Tier rate of
$0.23/GiB for a more expensive destination, that is roughly $5 of outbound
traffic. This is **not a Google Cloud dollar spending cap**: fixed VM/disk/IP
charges, taxes, existing usage, SSH/background traffic, other resources and
future price changes are separate. A root compromise or administrator changing
the rule is also outside this protection.

Verify the limiter with a small disposable HTTPS transfer and inspect its drop
counter, then verify HTTPS health and a complete host/join flow. Also verify
that the service dependency and rule survive a reboot during a maintenance
window. Never exempt public response traffic from the cap to improve latency.

## Other cost controls

- Keep one fixed-size VM and fixed-size disk; do not enable automatic scaling.
- Disable the guest log/metric exporter if it is not required. Native Compute
  Engine platform metrics remain available without the guest agent. Otherwise,
  explicitly bound log ingestion and high-cardinality metrics before exporting.
- Exclude this VM's guest logs from billable Cloud Logging sinks as a second
  line of defense. Keep required audit logs and local, size-limited journals.
- Turn off public nginx access logs; suppress routine reject/error spam. Bound
  journal usage, service memory, task count and CPU usage.
- Set a project-filtered monthly billing budget with actual and forecast
  thresholds. Budget alerts are delayed notifications, not the traffic guard.
- Keep public firewall ports limited to HTTPS, ACME and the legacy upgrade
  response. Keep operational access controlled separately.

Pricing and service coverage must be rechecked when changing the deployment:

- [Google Cloud network pricing](https://cloud.google.com/vpc/network-pricing)
- [Google Cloud spend cap coverage](https://cloud.google.com/billing/docs/how-to/budgets-spend-caps)
- [nftables rate limiter](https://netfilter.org/projects/nftables/manpage.html)
