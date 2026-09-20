#!/usr/bin/env bash
# Starts one BIP300 regtest stack here and one on a remote host, and joins them.
#
# The peer link goes through an SSH tunnel, so the remote host opens no port.
# Both stacks use their own data directory and their own ports.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REMOTE="${REMOTE:-alphanet}"
REMOTE_DIR="${REMOTE_DIR:-sol-drivechain-regtest}"
# The alphanet host keeps its patched build under the network name, not under
# `drivechain-patched`.
# The remote host keeps only eCash builds, and those hold another regtest
# chain. So the script installs stock Bitcoin Core of the same version and
# starts it with `acceptnonstdtxn`, the way the enforcer test harness does.
CORE_VERSION="${CORE_VERSION:-30.2}"
REMOTE_BITCOIND="${REMOTE_BITCOIND:-\$HOME/$REMOTE_DIR/bin/bitcoind}"
REMOTE_BITCOIN_CLI="${REMOTE_BITCOIN_CLI:-\$HOME/$REMOTE_DIR/bin/bitcoin-cli}"
REMOTE_ENFORCER="${REMOTE_ENFORCER:-\$HOME/.local/share/bitwindow/assets/bin/bip300301-enforcer}"

P2P_PORT="${P2P_PORT:-19444}"
TUNNEL_PORT="${TUNNEL_PORT:-19556}"

remote_stack() {
  ssh "$REMOTE" "ROOT=\$HOME/$REMOTE_DIR BITCOIND=$REMOTE_BITCOIND \
    BITCOIN_CLI=$REMOTE_BITCOIN_CLI ENFORCER=$REMOTE_ENFORCER ACCEPT_NONSTD=1 \
    bash \$HOME/$REMOTE_DIR/regtest.sh $*"
}

cleanup() {
  if [ -n "${TUNNEL_PID:-}" ]; then
    kill "$TUNNEL_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

echo "== the local stack =="
bash "$HERE/regtest.sh" start

echo
echo "== the remote stack on $REMOTE =="
ssh "$REMOTE" "bash -s" <<REMOTE_SETUP
set -euo pipefail
mkdir -p "\$HOME/$REMOTE_DIR/bin"
if [ ! -x "\$HOME/$REMOTE_DIR/bin/bitcoind" ]; then
  cd "\$HOME/$REMOTE_DIR"
  curl -sSfL -o core.tar.gz \
    "https://bitcoincore.org/bin/bitcoin-core-$CORE_VERSION/bitcoin-$CORE_VERSION-x86_64-linux-gnu.tar.gz"
  tar -xzf core.tar.gz
  cp "bitcoin-$CORE_VERSION/bin/bitcoind" "bitcoin-$CORE_VERSION/bin/bitcoin-cli" bin/
  rm -rf core.tar.gz "bitcoin-$CORE_VERSION"
fi
REMOTE_SETUP
scp -q "$HERE/regtest.sh" "$REMOTE:$REMOTE_DIR/regtest.sh"
remote_stack start

echo
echo "== the tunnel =="
ssh -f -N -L "$TUNNEL_PORT:127.0.0.1:$P2P_PORT" "$REMOTE"
TUNNEL_PID=$(pgrep -f "ssh -f -N -L $TUNNEL_PORT:127.0.0.1:$P2P_PORT" | head -1)
sleep 2
bash "$HERE/regtest.sh" cli addnode "127.0.0.1:$TUNNEL_PORT" onetry

echo
echo "== the check =="
for _ in $(seq 1 20); do
  peers=$(bash "$HERE/regtest.sh" cli getconnectioncount)
  [ "$peers" -gt 0 ] && break
  sleep 1
done
echo "local peers:  $(bash "$HERE/regtest.sh" cli getconnectioncount)"
echo "remote peers: $(remote_stack cli getconnectioncount)"

before=$(remote_stack cli getblockcount)
address=$(bash "$HERE/regtest.sh" cli -rpcwallet=miner getnewaddress 2>/dev/null || {
  bash "$HERE/regtest.sh" cli createwallet miner >/dev/null
  bash "$HERE/regtest.sh" cli -rpcwallet=miner getnewaddress
})
bash "$HERE/regtest.sh" cli generatetoaddress 5 "$address" >/dev/null
sleep 4
after=$(remote_stack cli getblockcount)

echo "local height:  $(bash "$HERE/regtest.sh" cli getblockcount)"
echo "remote height: $after (it was $before)"
if [ "$after" -gt "$before" ]; then
  echo "PASS: the remote node took the blocks that this host mined."
else
  echo "FAIL: the remote height did not grow." >&2
  exit 1
fi
