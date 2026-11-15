#!/usr/bin/env bash
set -euo pipefail

# Run on the rendezvous VM. Cap public HTTP/HTTPS responses, including TLS
# handshakes, errors and retransmissions, without throttling SSH or other apps.
if [[ "${EUID}" -ne 0 ]]; then
  echo "run as root on the rendezvous VM" >&2
  exit 1
fi
command -v nft >/dev/null

install -d -m 0755 /etc/skyffla /usr/local/sbin
cat > /etc/skyffla/cost-guard.nft <<'EOF'
table inet skyffla_cost_guard {
    chain output {
        type filter hook output priority 100; policy accept;
        oifname != "lo" tcp sport { 80, 443, 8080 } limit rate over 8 kbytes/second burst 128 kbytes counter drop
    }
}
EOF

cat > /usr/local/sbin/skyffla-cost-guard <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
rules=$(mktemp)
trap 'rm -f "$rules"' EXIT
if /usr/sbin/nft list table inet skyffla_cost_guard >/dev/null 2>&1; then
  echo 'delete table inet skyffla_cost_guard' > "$rules"
fi
cat /etc/skyffla/cost-guard.nft >> "$rules"
/usr/sbin/nft --check --file "$rules"
# Delete and recreate only our table in one atomic netlink transaction.
/usr/sbin/nft --file "$rules"
EOF
chmod 0755 /usr/local/sbin/skyffla-cost-guard

cat > /etc/systemd/system/skyffla-cost-guard.service <<'EOF'
[Unit]
Description=Bound public rendezvous outbound traffic costs
DefaultDependencies=no
After=local-fs.target systemd-modules-load.service nftables.service
Before=network-pre.target nginx.service
Wants=network-pre.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/skyffla-cost-guard
RemainAfterExit=yes
# Deliberately retain the kernel rules on stop; stopping this unit stops nginx.

[Install]
WantedBy=multi-user.target
EOF

install -d -m 0755 /etc/systemd/system/nginx.service.d
cat > /etc/systemd/system/nginx.service.d/cost-guard.conf <<'EOF'
[Unit]
Requires=skyffla-cost-guard.service
After=skyffla-cost-guard.service
EOF

systemctl daemon-reload
systemctl enable skyffla-cost-guard.service
systemctl restart skyffla-cost-guard.service
nft list table inet skyffla_cost_guard
