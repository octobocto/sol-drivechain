#!/usr/bin/env bash
# Starts each unit in genesis/systemd with a probe in place of its program,
# and checks what the sandbox lets the probe read and write. Run it as root
# on a disposable Linux host with systemd, such as a CI runner.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TREE=/root/src/sol-drivechain
UNITS=(sol-validator sol-bmm sol-peg sol-regtest)

if [ "$(id -u)" != 0 ]; then
  echo "error: run this as root." >&2
  exit 1
fi
if [ -e "$TREE" ]; then
  echo "error: $TREE exists. Run this test only on a disposable host." >&2
  exit 1
fi
for unit in "${UNITS[@]}"; do
  if [ -e "/etc/systemd/system/$unit.service" ]; then
    echo "error: $unit.service is installed. Run this test only on a disposable host." >&2
    exit 1
  fi
done

cleanup() {
  for unit in "${UNITS[@]}"; do
    systemctl stop "$unit" >/dev/null 2>&1 || true
    rm -f "/etc/systemd/system/$unit.service"
  done
  systemctl daemon-reload
  rm -rf "$TREE" /root/sol-regtest
}
trap cleanup EXIT

mkdir -p "$TREE/keys" "$TREE/genesis/ledger" "$TREE/daemon/target/release" "$TREE/scripts"
for key in validator-identity.json validator-vote.json oracle.json oracle.bundles.json \
  faucet.json user.json mnemonic.txt bridge-program.pubkey; do
  echo secret >"$TREE/keys/$key"
done

PROBE="$TREE/probe.sh"
cat >"$PROBE" <<'EOF'
#!/usr/bin/env bash
TREE=/root/src/sol-drivechain
for key in validator-identity.json validator-vote.json oracle.json oracle.bundles.json \
  faucet.json user.json mnemonic.txt bridge-program.pubkey; do
  if cat "$TREE/keys/$key" >/dev/null 2>&1; then echo "PROBE read $key yes"; else echo "PROBE read $key no"; fi
done
write() {
  if : >"$2/.probe" 2>/dev/null && rm -f "$2/.probe"; then echo "PROBE write $1 yes"; else echo "PROBE write $1 no"; fi
}
write etc /etc
write usr /usr
write keys "$TREE/keys"
write ledger "$TREE/genesis/ledger"
mkdir -p /root/sol-regtest 2>/dev/null
write regtest /root/sol-regtest
if echo new >"$TREE/keys/oracle.bundles.new" 2>/dev/null \
  && mv "$TREE/keys/oracle.bundles.new" "$TREE/keys/oracle.bundles.json" 2>/dev/null; then
  echo "PROBE rename bundles yes"
else
  echo "PROBE rename bundles no"
fi
echo "PROBE nonewprivs $(awk '/^NoNewPrivs:/ {print $2}' /proc/self/status)"
echo "PROBE capeff $(awk '/^CapEff:/ {print $2}' /proc/self/status)"
echo "PROBE done"
EOF
chmod +x "$PROBE"
install -m 755 "$PROBE" "$TREE/genesis/run-validator.sh"
install -m 755 "$PROBE" "$TREE/daemon/target/release/sol-drivechain-daemon"
install -m 755 "$PROBE" "$TREE/scripts/regtest-chain.sh"
printf 'sleep infinity\n' >>"$TREE/genesis/run-validator.sh"
printf 'sleep infinity\n' >>"$TREE/daemon/target/release/sol-drivechain-daemon"

for unit in "${UNITS[@]}"; do
  install -m 644 "$REPO/genesis/systemd/$unit.service" /etc/systemd/system/
done
systemctl daemon-reload

FAILED=0
probe_of() {
  local unit="$1" since="$2"
  for _ in $(seq 1 30); do
    if journalctl -u "$unit" --since "$since" -o cat --no-pager | grep -q '^PROBE done$'; then
      journalctl -u "$unit" --since "$since" -o cat --no-pager | grep '^PROBE '
      return 0
    fi
    sleep 1
  done
  echo "error: $unit gave no probe result." >&2
  systemctl status "$unit" --no-pager >&2 || true
  journalctl -u "$unit" --since "$since" -o cat --no-pager >&2 || true
  return 1
}

check() {
  local unit="$1"
  shift
  local since
  since="$(date '+%Y-%m-%d %H:%M:%S')"
  sleep 1
  echo secret >"$TREE/keys/oracle.bundles.json"
  if ! systemctl start "$unit"; then
    echo "FAIL $unit did not start" >&2
    journalctl -u "$unit" --since "$since" -o cat --no-pager >&2 || true
    FAILED=1
    return
  fi
  local result
  if ! result="$(probe_of "$unit" "$since")"; then
    FAILED=1
    systemctl stop "$unit" || true
    return
  fi
  systemctl stop "$unit" || true
  for want in "$@"; do
    if grep -qx "PROBE $want" <<<"$result"; then
      echo "ok   $unit: $want"
    else
      echo "FAIL $unit: want '$want', got '$(grep "^PROBE ${want% *} " <<<"$result")'" >&2
      FAILED=1
    fi
  done
}

check sol-validator \
  "read validator-identity.json yes" "read validator-vote.json yes" "read bridge-program.pubkey yes" \
  "read oracle.json no" "read oracle.bundles.json no" "read mnemonic.txt no" \
  "read faucet.json no" "read user.json no" \
  "write etc no" "write usr no" "write keys no" "write ledger yes" "nonewprivs 1"

check sol-bmm \
  "read validator-identity.json yes" "read bridge-program.pubkey yes" \
  "read validator-vote.json no" "read oracle.json no" "read oracle.bundles.json no" \
  "read mnemonic.txt no" "read faucet.json no" "read user.json no" \
  "write etc no" "write keys no" "write ledger no" "write regtest no" \
  "nonewprivs 1" "capeff 0000000000000000"

check sol-peg \
  "read oracle.json yes" "read oracle.bundles.json yes" "read bridge-program.pubkey yes" \
  "read validator-identity.json no" "read validator-vote.json no" "read mnemonic.txt no" \
  "read faucet.json no" "read user.json no" \
  "write etc no" "write ledger no" "write regtest no" "rename bundles yes" \
  "nonewprivs 1" "capeff 0000000000000000"

check sol-regtest "write etc no" "write usr no" "write regtest yes" "nonewprivs 1"

if [ "$FAILED" != 0 ]; then
  echo "error: a unit sandbox check failed." >&2
  exit 1
fi
echo "All unit sandbox checks passed."
