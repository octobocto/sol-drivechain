#!/usr/bin/env bash
# Runs a standing BIP300 regtest chain: a Bitcoin node, an enforcer, a Solana
# validator, the peg, and the BMM loop. A miner adds a block every minute, so
# bids win and settles pay, again and again.
#
# `scripts/regtest-peg.sh` proves the chain one time and takes it down. This
# script keeps it up, so an operator can watch it and send transactions to it.
#
#   bash scripts/regtest-chain.sh up
#   bash scripts/regtest-chain.sh status
#   bash scripts/regtest-chain.sh mine 5
#   bash scripts/regtest-chain.sh down
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

ROOT="${ROOT:-$HOME/sol-regtest}"
SLOT="${SLOT:-8}"
MINE_INTERVAL="${MINE_INTERVAL:-60}"
BMM_CONFIRMATIONS="${BMM_CONFIRMATIONS:-6}"

# The Bitcoin side. Every port sits away from the one-shot test and from any
# node of the host.
export RPC_PORT="${RPC_PORT:-19543}"
export P2P_PORT="${P2P_PORT:-19544}"
export ZMQ_PORT="${ZMQ_PORT:-19545}"
export GRPC_PORT="${GRPC_PORT:-19651}"
export ENFORCER_RPC_PORT="${ENFORCER_RPC_PORT:-19652}"
# The eCash builds keep an OP_DRIVECHAIN deposit out of the regtest mempool
# under the standard policy.
export ACCEPT_NONSTD="${ACCEPT_NONSTD:-1}"
# The read-only login of the Bitcoin node. A proxy can hand a builder these
# calls, and the login reaches no wallet and no write. The password is
# `reader`, because the chain carries no value.
export READER_RPC_AUTH="${READER_RPC_AUTH:-reader:9fe79e638922d8bd07832bd3f910c43d\$f8d68fb8dc25a6b1d198e512f8c9b31e7c3bd42394d8ce4bf282cce8a618be12}"

# The faucet of the chain. The RPC passes an airdrop call to it, so a browser
# asks the chain itself for test money. The faucet holds pegged BTC.
FAUCET_PORT="${FAUCET_PORT:-9900}"
FAUCET_BTC="${FAUCET_BTC:-500}"
FAUCET_REQUEST_CAP="${FAUCET_REQUEST_CAP:-2}"
FAUCET_TIME_CAP="${FAUCET_TIME_CAP:-200}"

# The Solana side.
SOLANA_RPC_PORT="${SOLANA_RPC_PORT:-8799}"
SOLANA_GOSSIP_PORT="${SOLANA_GOSSIP_PORT:-8201}"
# The RPC stays on loopback. An operator who wants builders on the chain binds
# 0.0.0.0 here.
SOLANA_RPC_BIND_ADDRESS="${SOLANA_RPC_BIND_ADDRESS:-127.0.0.1}"
DYNAMIC_PORT_RANGE="${DYNAMIC_PORT_RANGE:-8200-8230}"

ENFORCER_URL="http://127.0.0.1:$GRPC_PORT"
SOLANA_URL="http://127.0.0.1:$SOLANA_RPC_PORT"
E="--network regtest --enforcer-url $ENFORCER_URL --slot $SLOT"

D="${D:-$REPO/daemon/target/release/sol-drivechain-daemon}"
SOLANA="${SOLANA:-$(command -v solana || true)}"
SOLANA_KEYGEN="${SOLANA_KEYGEN:-$(command -v solana-keygen || true)}"
SOLANA_FAUCET="${SOLANA_FAUCET:-$(command -v solana-faucet || true)}"
export SOLANA_GENESIS="${SOLANA_GENESIS:-$HOME/src/agave/target/release/solana-genesis}"
export AGAVE_VALIDATOR="${AGAVE_VALIDATOR:-$HOME/src/agave/target/release/agave-validator}"

RUN="$ROOT/run"
fail() { echo "error: $1" >&2; exit 1; }
first_line() { printf '%s\n' "$1" | sed -n 1p; }
btc() { ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" cli "$@"; }
new_address() { first_line "$("$D" wallet-address $E)"; }
program_id() { cat "$ROOT/keys/bridge-program.pubkey"; }

# Starts one background process and writes its pid. A second call does nothing
# while the first one runs.
start_process() {
  local name="$1"
  shift
  local pid_file="$RUN/$name.pid"
  if [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
    echo "$name already runs"
    return 0
  fi
  mkdir -p "$RUN"
  nohup "$@" > "$ROOT/$name.log" 2>&1 &
  echo $! > "$pid_file"
  echo "$name runs as $(cat "$pid_file")"
}

stop_process() {
  local name="$1"
  local pid_file="$RUN/$name.pid"
  [ -f "$pid_file" ] || return 0
  local pid
  pid="$(cat "$pid_file")"
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$pid_file"
  echo "$name stopped"
}

is_running() {
  local pid_file="$RUN/$1.pid"
  [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null
}

wait_for_solana() {
  for _ in $(seq 1 90); do
    if curl -s --max-time 5 -X POST -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$SOLANA_URL" \
        2>/dev/null | grep -q '"result":"ok"'; then
      return 0
    fi
    is_running validator || fail "the validator stopped. See $ROOT/validator.log"
    sleep 2
  done
  fail "the Solana chain did not become healthy. See $ROOT/validator.log"
}

slot_is_active() {
  "$D" status $E 2>/dev/null | grep -qE "^slot $SLOT .* active at"
}

claim_slot() {
  slot_is_active && { echo "slot $SLOT is active"; return 0; }
  echo "claiming slot $SLOT"
  local address
  address="$(new_address)"
  "$D" propose-slot $E >/dev/null
  "$D" mine $E --blocks 1 --address "$address" >/dev/null
  "$D" ack-slot $E >/dev/null
  "$D" mine $E --blocks 6 --address "$address" >/dev/null
  slot_is_active || fail "slot $SLOT did not activate"
  echo "slot $SLOT is active"
}

# Mines until the enforcer wallet holds the coins of the faucet and a margin.
# A coinbase takes 100 blocks to mature, so a fresh chain mines a few hundred.
fund_wallet() {
  local want balance
  want=$(( (FAUCET_BTC + 10) * 100000000 ))
  for _ in $(seq 1 30); do
    balance="$("$D" wallet-balance $E 2>/dev/null | awk '/^confirmed/ {print $2}')"
    if [ "${balance:-0}" -ge "$want" ]; then
      echo "the enforcer wallet holds $balance sats"
      return 0
    fi
    "$D" mine $E --blocks 25 --address "$(new_address)" >/dev/null
    sleep 1
  done
  fail "the enforcer wallet holds ${balance:-0} sats, and the faucet asks for $want"
}

# Pegs BTC into the faucet key. The faucet then pays out coins that a deposit
# on eCash backs, exactly like every other coin of the chain.
fund_faucet() {
  local pubkey balance want
  pubkey="$("$SOLANA_KEYGEN" pubkey "$ROOT/keys/faucet.json")"
  want=$((FAUCET_BTC * 100000000))
  balance="$("$SOLANA" -u "$SOLANA_URL" balance "$pubkey" --lamports 2>/dev/null | awk '{print $1}')"
  if [ "${balance:-0}" -gt $((want * 10 / 2)) ]; then
    echo "the faucet holds $balance lamports"
    return 0
  fi
  echo "pegging $FAUCET_BTC BTC into the faucet $pubkey"
  "$D" deposit $E --pubkey "$pubkey" --sats "$want" >/dev/null
  "$D" mine $E --blocks 2 --address "$(new_address)" >/dev/null
  for _ in $(seq 1 40); do
    balance="$("$SOLANA" -u "$SOLANA_URL" balance "$pubkey" --lamports 2>/dev/null | awk '{print $1}')"
    [ "${balance:-0}" -ge $((want * 10)) ] && break
    sleep 3
  done
  [ "${balance:-0}" -ge $((want * 10)) ] || fail "the faucet holds ${balance:-0} lamports, and the peg in asked for $((want * 10))"
  echo "the faucet holds $balance lamports of pegged BTC"
}

mine_loop() {
  while true; do
    "$D" mine $E --blocks 1 --address "$(new_address)" >/dev/null 2>&1 || true
    sleep "$MINE_INTERVAL"
  done
}

up() {
  for binary in "$D" "$SOLANA" "$SOLANA_KEYGEN" "$SOLANA_GENESIS" "$AGAVE_VALIDATOR"; do
    [ -x "$binary" ] || fail "$binary is missing"
  done
  mkdir -p "$ROOT/keys" "$RUN"
  cp -n "$REPO/keys/bridge-program.pubkey" "$ROOT/keys/" 2>/dev/null || true

  echo "== the Bitcoin node and the enforcer"
  ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" start

  echo "== the sidechain slot"
  claim_slot
  fund_wallet

  echo "== the Solana chain"
  if [ ! -d "$ROOT/ledger" ]; then
    LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" bash "$REPO/genesis/build-genesis.sh" >/dev/null
  fi
  start_process validator env \
    LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" RPC_PORT="$SOLANA_RPC_PORT" \
    GOSSIP_PORT="$SOLANA_GOSSIP_PORT" DYNAMIC_PORT_RANGE="$DYNAMIC_PORT_RANGE" \
    XDP=0 RPC_BIND_ADDRESS="$SOLANA_RPC_BIND_ADDRESS" \
    ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" \
    FAUCET_ADDRESS="127.0.0.1:$FAUCET_PORT" \
    BMM_CONFIRMATIONS="$BMM_CONFIRMATIONS" AGAVE_VALIDATOR="$AGAVE_VALIDATOR" \
    bash "$REPO/genesis/run-validator.sh"
  wait_for_solana
  echo "the Solana chain runs at slot $("$SOLANA" -u "$SOLANA_URL" slot)"

  echo "== the bridge"
  if ! "$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$(program_id)" \
      >/dev/null 2>&1; then
    "$D" initialize --solana-rpc-url "$SOLANA_URL" --program-id "$(program_id)" \
      --payer "$ROOT/keys/oracle.json" --oracle "$ROOT/keys/oracle.json" \
      --bmm-start-height "$(btc getblockcount)"
  fi

  echo "== the peg, the BMM loop, and the miner"
  start_process peg "$D" run --network regtest --enforcer-url "$ENFORCER_URL" \
    --solana-rpc-url "$SOLANA_URL" --slot "$SLOT" --program-id "$(program_id)" \
    --oracle "$ROOT/keys/oracle.json" --confirmations 1 --bundle-interval-secs 10
  start_process bmm "$D" bmm --enforcer-url "$ENFORCER_URL" \
    --solana-rpc-url "$SOLANA_URL" --slot "$SLOT" --program-id "$(program_id)" \
    --identity "$ROOT/keys/validator-identity.json" \
    --confirmations "$BMM_CONFIRMATIONS" --min-bid-sats 1 --interval-secs 2
  start_process miner bash "$0" mine-loop

  echo "== the faucet"
  fund_faucet
  if [ -n "$SOLANA_FAUCET" ]; then
    start_process faucet "$SOLANA_FAUCET" --keypair "$ROOT/keys/faucet.json" \
      --per-request-cap "$FAUCET_REQUEST_CAP" --per-time-cap "$FAUCET_TIME_CAP"
  else
    echo "solana-faucet is missing, so the chain gives no airdrop"
  fi
  status
}

down() {
  stop_process faucet
  stop_process miner
  stop_process bmm
  stop_process peg
  stop_process validator
  ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" stop || true
}

status() {
  echo
  echo "root          $ROOT"
  for name in validator peg bmm miner faucet; do
    if is_running "$name"; then
      echo "$name        runs"
    else
      echo "$name        down"
    fi
  done
  echo "eCash height  $(btc getblockcount 2>/dev/null || echo unknown)"
  if [ -n "$SOLANA" ] && "$SOLANA" -u "$SOLANA_URL" slot >/dev/null 2>&1; then
    echo "Solana slot   $("$SOLANA" -u "$SOLANA_URL" slot)"
    "$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$(program_id)" \
      2>/dev/null || true
  fi
  if [ -f "$ROOT/bmm.log" ]; then
    echo "last BMM win  $(grep -c 'settled its own win' "$ROOT/bmm.log" || true) so far"
    grep 'settled its own win' "$ROOT/bmm.log" | tail -1 || true
  fi
  echo
  echo "rpc           $SOLANA_URL (bound on $SOLANA_RPC_BIND_ADDRESS)"
  echo "enforcer      $ENFORCER_URL"
  echo "faucet        127.0.0.1:$FAUCET_PORT"
}

case "${1:-status}" in
  up) up ;;
  down) down ;;
  status) status ;;
  mine) "$D" mine $E --blocks "${2:-1}" --address "$(new_address)" ;;
  mine-loop) mine_loop ;;
  *)
    echo "usage: $0 {up|down|status|mine [blocks]}" >&2
    exit 1
    ;;
esac
