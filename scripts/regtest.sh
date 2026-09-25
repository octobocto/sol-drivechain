#!/usr/bin/env bash
# Starts, stops, or reports one local BIP300 regtest stack.
#
# The stack is a drivechain-patched bitcoind plus the enforcer. Both use their
# own data directory and their own ports, so they never touch a node that
# already runs on this host.
set -euo pipefail

ROOT="${ROOT:-$HOME/.sol-drivechain/regtest}"
RPC_PORT="${RPC_PORT:-19443}"
P2P_PORT="${P2P_PORT:-19444}"
ZMQ_PORT="${ZMQ_PORT:-19445}"
GRPC_PORT="${GRPC_PORT:-19551}"
ENFORCER_RPC_PORT="${ENFORCER_RPC_PORT:-19552}"
RPC_USER="${RPC_USER:-peg}"
RPC_PASS="${RPC_PASS:-peg}"
# Stock Core does not relay an OP_DRIVECHAIN output. Set this to 1 when the
# node is an unpatched build, the way the enforcer test harness does.
ACCEPT_NONSTD="${ACCEPT_NONSTD:-0}"

case "$(uname -s)" in
  Darwin) ASSETS="${ASSETS:-$HOME/Library/Application Support/bitwindow/assets/bin}" ;;
  *) ASSETS="${ASSETS:-$HOME/.local/share/bitwindow/assets/bin}" ;;
esac

# The patched build treats an OP_DRIVECHAIN output as standard. Stock Core does
# not relay one, so a deposit never reaches a block.
BITCOIND="${BITCOIND:-$ASSETS/drivechain-patched/bitcoind}"
BITCOIN_CLI="${BITCOIN_CLI:-$ASSETS/drivechain-patched/bitcoin-cli}"
ENFORCER="${ENFORCER:-$ASSETS/bip300301-enforcer}"

for binary in "$BITCOIND" "$BITCOIN_CLI" "$ENFORCER"; do
  if [ ! -x "$binary" ]; then
    echo "error: $binary is missing. Set ASSETS, BITCOIND, or ENFORCER." >&2
    exit 1
  fi
done

NODE_DIR="$ROOT/bitcoin"
ENFORCER_DIR="$ROOT/enforcer"

cli() {
  "$BITCOIN_CLI" -regtest -datadir="$NODE_DIR" -rpcport="$RPC_PORT" \
    -rpcuser="$RPC_USER" -rpcpassword="$RPC_PASS" "$@"
}

start_node() {
  mkdir -p "$NODE_DIR"
  cat > "$NODE_DIR/bitcoin.conf" <<EOF
regtest=1
server=1
daemon=1
txindex=1
rpcuser=$RPC_USER
rpcpassword=$RPC_PASS
fallbackfee=0.0001
[regtest]
rpcport=$RPC_PORT
port=$P2P_PORT
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
bind=127.0.0.1
zmqpubsequence=tcp://127.0.0.1:$ZMQ_PORT
EOF
  if [ "$ACCEPT_NONSTD" = "1" ]; then
    echo "acceptnonstdtxn=1" >> "$NODE_DIR/bitcoin.conf"
  fi
  if cli getblockcount >/dev/null 2>&1; then
    echo "bitcoind already runs at height $(cli getblockcount)"
    return 0
  fi
  "$BITCOIND" -datadir="$NODE_DIR" -conf="$NODE_DIR/bitcoin.conf"
  for _ in $(seq 1 40); do
    if cli getblockcount >/dev/null 2>&1; then
      echo "bitcoind runs at height $(cli getblockcount)"
      return 0
    fi
    sleep 1
  done
  echo "error: bitcoind did not answer its RPC." >&2
  return 1
}

start_enforcer() {
  mkdir -p "$ENFORCER_DIR"
  if nc -z 127.0.0.1 "$GRPC_PORT" 2>/dev/null; then
    echo "the enforcer already listens on $GRPC_PORT"
    return 0
  fi
  # `--wallet-sync-source disabled` keeps the wallet on live blocks only, so
  # the stack needs no electrs and no esplora.
  nohup "$ENFORCER" \
    --data-dir="$ENFORCER_DIR" \
    --node-rpc-addr=127.0.0.1:"$RPC_PORT" \
    --node-rpc-user="$RPC_USER" \
    --node-rpc-pass="$RPC_PASS" \
    --node-zmq-addr-sequence=tcp://127.0.0.1:"$ZMQ_PORT" \
    --serve-grpc-addr=127.0.0.1:"$GRPC_PORT" \
    --serve-rpc-addr=127.0.0.1:"$ENFORCER_RPC_PORT" \
    --enable-wallet \
    --wallet-auto-create \
    --wallet-sync-source=disabled \
    --log-level=info \
    >> "$ENFORCER_DIR/enforcer.log" 2>&1 &
  for _ in $(seq 1 60); do
    if nc -z 127.0.0.1 "$GRPC_PORT" 2>/dev/null; then
      echo "the enforcer serves grpc on 127.0.0.1:$GRPC_PORT"
      return 0
    fi
    sleep 1
  done
  echo "error: the enforcer did not open its grpc port. See $ENFORCER_DIR/enforcer.log" >&2
  tail -20 "$ENFORCER_DIR/enforcer.log" >&2 || true
  return 1
}

case "${1:-status}" in
  start)
    start_node
    start_enforcer
    ;;
  stop)
    pkill -f "bip300301-enforcer --data-dir=$ENFORCER_DIR" 2>/dev/null || true
    cli stop 2>/dev/null || true
    echo "stopped"
    ;;
  status)
    if cli getblockcount >/dev/null 2>&1; then
      echo "bitcoind height: $(cli getblockcount)"
      echo "bitcoind peers:  $(cli getconnectioncount)"
    else
      echo "bitcoind does not run"
    fi
    if nc -z 127.0.0.1 "$GRPC_PORT" 2>/dev/null; then
      echo "enforcer grpc:   127.0.0.1:$GRPC_PORT"
    else
      echo "the enforcer does not run"
    fi
    ;;
  logs)
    tail -f "$ENFORCER_DIR/enforcer.log"
    ;;
  cli)
    shift
    cli "$@"
    ;;
  *)
    echo "use: $0 [start|stop|status|logs|cli <args>]" >&2
    exit 1
    ;;
esac
