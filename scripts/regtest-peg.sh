#!/usr/bin/env bash
# Proves the whole peg on regtest: a slot claim, a peg in, a peg out, and a
# BMM win that pays the treasury to a validator. Last, it halts the chain and
# migrates it into a new genesis.
#
# It builds its own Bitcoin node, its own enforcer, and its own Solana chain,
# all on separate ports and separate directories. It never touches a chain that
# already runs on this host.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

ROOT="${ROOT:-$HOME/peg-regtest}"
SLOT="${SLOT:-8}"
DEPOSIT_SATS="${DEPOSIT_SATS:-5000000}"
WITHDRAW_SATS="${WITHDRAW_SATS:-2000000}"
WITHDRAW_FEE_SATS="${WITHDRAW_FEE_SATS:-10000}"
# N: the eCash blocks on top of a block before its settle. The validators and
# the BMM loop must use the same value.
BMM_CONFIRMATIONS="${BMM_CONFIRMATIONS:-6}"
# D: the eCash confirmations before a credit. It stays above the deepest
# reorg of this test, 9 blocks, so no reorg here drops a credited deposit.
DEPOSIT_CONFIRMATIONS="${DEPOSIT_CONFIRMATIONS:-10}"
# The eCash builds keep an OP_DRIVECHAIN deposit out of the regtest mempool
# under the standard policy, so the node takes a nonstandard tx.
export ACCEPT_NONSTD="${ACCEPT_NONSTD:-1}"

# The public BIP39 test phrase, the same one the seed tests pin.
TEST_MNEMONIC="${TEST_MNEMONIC:-abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about}"

# The Bitcoin side. Its ports sit away from every default.
export RPC_PORT="${RPC_PORT:-19443}"
export P2P_PORT="${P2P_PORT:-19444}"
export ZMQ_PORT="${ZMQ_PORT:-19445}"
export GRPC_PORT="${GRPC_PORT:-19551}"
export ENFORCER_RPC_PORT="${ENFORCER_RPC_PORT:-19552}"

# The Solana side.
SOLANA_RPC_PORT="${SOLANA_RPC_PORT:-8999}"
SOLANA_GOSSIP_PORT="${SOLANA_GOSSIP_PORT:-8101}"
# The JSON-RPC that drivechaind calls to bid.
BMM_RPC_PORT="${BMM_RPC_PORT:-19560}"

ENFORCER_URL="http://127.0.0.1:$GRPC_PORT"
SOLANA_URL="http://127.0.0.1:$SOLANA_RPC_PORT"
E="--network regtest --enforcer-url $ENFORCER_URL --slot $SLOT"

D="${D:-$REPO/daemon/target/release/sol-drivechain-daemon}"
SOLANA="${SOLANA:-$(command -v solana)}"
SOLANA_KEYGEN="${SOLANA_KEYGEN:-$(command -v solana-keygen)}"
export SOLANA_GENESIS="${SOLANA_GENESIS:-$HOME/src/agave/target/release/solana-genesis}"
export AGAVE_VALIDATOR="${AGAVE_VALIDATOR:-$HOME/src/agave/target/release/agave-validator}"

step() { printf '\n== %s ==\n' "$1"; }
btc() { ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" cli "$@"; }
# The payout address belongs to the enforcer wallet, so bitcoind cannot answer
# a wallet call about it. `scantxoutset` reads the UTXO set of the node.
received() {
  btc scantxoutset start "[\"addr($1)\"]" | sed -n 's/.*"total_amount": \([0-9.]*\).*/\1/p'
}
fail() { echo "FAIL: $1" >&2; exit 1; }
rpc() {
  curl -s --max-time 5 -X POST -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" "$SOLANA_URL"
}
# The total supply of the chain, and the slot of the read.
supply() {
  rpc getSupply '[{"commitment":"processed","excludeNonCirculatingAccountsList":true}]' \
    | python3 -c 'import json,sys; r=json.load(sys.stdin)["result"]; print(r["value"]["total"], r["context"]["slot"])'
}
books() {
  "$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
    --books --enforcer-url "$ENFORCER_URL" --slot "$SLOT"
}
# The peg marks a paid withdrawal a few seconds after eCash pays it, and the
# books close after that.
books_close() {
  for _ in $(seq 1 30); do
    books > "$ROOT/books.log" 2>&1 && { cat "$ROOT/books.log"; return 0; }
    sleep 2
  done
  cat "$ROOT/books.log" >&2
  fail "the books do not close"
}

# A pipe into `head` closes the reader early, and the daemon then dies of a
# broken pipe. So the shell takes the whole output and cuts the first line.
first_line() { printf '%s\n' "$1" | sed -n 1p; }
new_address() { first_line "$("$D" wallet-address $E)"; }

# The enforcer follows the node. Right after a reorg its block template still
# names the old tip, and a mine call then fails. So every reorg waits here.
wait_for_enforcer() {
  for _ in $(seq 1 60); do
    if [ "$(btc getbestblockhash)" = "$("$D" ecash-tip $E | awk '{print $2}')" ]; then
      return 0
    fi
    sleep 2
  done
  fail "the enforcer did not reach the node tip"
}

for binary in "$D" "$SOLANA" "$SOLANA_KEYGEN" "$SOLANA_GENESIS" "$AGAVE_VALIDATOR"; do
  [ -x "$binary" ] || fail "$binary is missing"
done

# The script stops only the processes that it started.
PIDS=()
cleanup() {
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

step "a clean slate"
cleanup
sleep 3
rm -rf "$ROOT"
mkdir -p "$ROOT/keys"
cp "$REPO/keys/bridge-program.pubkey" "$ROOT/keys/"
PROGRAM_ID="$(cat "$ROOT/keys/bridge-program.pubkey")"

step "the Bitcoin node and the enforcer"
ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" start

step "the Solana chain"
# The genesis writes the bridge config: BMM settles from this eCash height,
# and the credits start after it.
BMM_START="$(btc getblockcount)"
LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" ECASH_START_HEIGHT="$BMM_START" \
  DEPOSIT_CONFIRMATIONS="$DEPOSIT_CONFIRMATIONS" \
  bash "$REPO/genesis/build-genesis.sh" > "$ROOT/genesis.log"
# A copy of the fresh ledger lets a second validator replay from the genesis,
# with no snapshot download.
cp -R "$ROOT/ledger" "$ROOT/ledger2"
LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" RPC_PORT="$SOLANA_RPC_PORT" \
  GOSSIP_PORT="$SOLANA_GOSSIP_PORT" XDP=0 \
  ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" \
  BMM_CONFIRMATIONS="$BMM_CONFIRMATIONS" ALLOW_PRIVATE_ADDR=1 \
  nohup bash "$REPO/genesis/run-validator.sh" > "$ROOT/validator.log" 2>&1 &
VALIDATOR_PID=$!
PIDS+=("$VALIDATOR_PID")
# The stock runtime creates the rent of its sysvars at slots 0 and 1. The
# first block record write is at slot 32. The supply between them must hold to
# the end, so the patch mints no rent.
SUPPLY_START=""
for _ in $(seq 1 120); do
  SUPPLY_START="$(supply 2>/dev/null || true)"
  [ -n "$SUPPLY_START" ] && [ "${SUPPLY_START#* }" -ge 2 ] && break
  sleep 0.2
done
[ -n "$SUPPLY_START" ] || fail "the RPC never gave the supply"
[ "${SUPPLY_START#* }" -lt 32 ] || fail "the first supply read came at slot ${SUPPLY_START#* }, after the first record write"
echo "the supply is ${SUPPLY_START% *} lamports at slot ${SUPPLY_START#* }"
# The RPC answers before the node is healthy, and an early transaction fails
# with "Node is unhealthy".
HEALTHY=no
for _ in $(seq 1 90); do
  if curl -s --max-time 5 -X POST -H 'content-type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$SOLANA_URL" \
      2>/dev/null | grep -q '"result":"ok"'; then
    HEALTHY=yes
    break
  fi
  # A dead validator never answers, so the wait must stop at once.
  if ! kill -0 "$VALIDATOR_PID" 2>/dev/null; then
    fail "the validator stopped before it became healthy"
  fi
  sleep 2
done
[ "$HEALTHY" = yes ] || fail "the Solana chain did not become healthy"
echo "the Solana chain runs at slot $("$SOLANA" -u "$SOLANA_URL" slot)"
SLOTS_PER_EPOCH="$(rpc getEpochSchedule '[]' | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["slotsPerEpoch"])')"
[ "$SLOTS_PER_EPOCH" = 432000 ] || fail "the genesis holds $SLOTS_PER_EPOCH slots per epoch, not 432000"
echo "PASS: the genesis holds 432000 slots per epoch"

step "the bridge config and the block record come from the genesis"
"$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID"
[ "$("$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  | awk '/^credited height/ {print $3}')" = "$BMM_START" ] || fail "the genesis config does not start at eCash height $BMM_START"
RECORD="$(rpc getAccountInfo '["BmmB1ocks1111111111111111111111111111111111",{"encoding":"base64","dataSlice":{"offset":0,"length":0}}]' \
  | python3 -c 'import json,sys; v=json.load(sys.stdin)["result"]["value"]; print(v["lamports"], v["owner"], v["space"])')"
[ "$RECORD" = "$(rpc getMinimumBalanceForRentExemption '[229376]' | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"])') Sysvar1111111111111111111111111111111111111 229376" ] \
  || fail "the block record is $RECORD"
echo "PASS: the genesis holds the config, and the block record is rent-exempt at its size: $RECORD"

step "claim sidechain slot $SLOT"
"$D" propose-slot $E >/dev/null
COINBASE="$(new_address)"
"$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
"$D" ack-slot $E >/dev/null
"$D" mine $E --blocks 6 --address "$COINBASE" >/dev/null
"$D" status $E | grep -E "^slot $SLOT " || fail "slot $SLOT did not activate"

step "fund the enforcer wallet"
"$D" mine $E --blocks 101 --address "$COINBASE" >/dev/null
sleep 3

step "a wallet from the BitWindow seed phrase"
# The public BIP39 test phrase. It holds no money anywhere.
MNEMONIC_FILE="$ROOT/keys/mnemonic.txt"
printf '%s\n' "$TEST_MNEMONIC" > "$MNEMONIC_FILE"
chmod 600 "$MNEMONIC_FILE"
USER_KEY="$ROOT/keys/user.json"
"$D" seed-keypair --mnemonic-file "$MNEMONIC_FILE" --account 0 --out "$USER_KEY" >/dev/null
USER_PUBKEY="$("$SOLANA_KEYGEN" pubkey "$USER_KEY")"
echo "the user is $USER_PUBKEY, from account 0 of the seed phrase"

# `solana-keygen` must read the same file, so every Solana tool accepts it.
SEED_PUBKEY="$("$D" seed-pubkey --mnemonic-file "$MNEMONIC_FILE" --account 0 | awk '{print $2}')"
[ "$SEED_PUBKEY" = "$USER_PUBKEY" ] || fail "the seed gives $SEED_PUBKEY, the keypair file gives $USER_PUBKEY"
echo "PASS: solana-keygen and the seed phrase name the same account"

step "peg in $DEPOSIT_SATS sats"

start_peg() {
  nohup "$D" run --network regtest --enforcer-url "$ENFORCER_URL" \
    --solana-rpc-url "$SOLANA_URL" --slot "$SLOT" --program-id "$PROGRAM_ID" \
    --oracle "$ROOT/keys/oracle.json" --bundle-interval-secs 10 \
    >> "$ROOT/peg.log" 2>&1 &
  PEG_PID=$!
  PIDS+=("$PEG_PID")
  sleep 5
}

lamports_of() {
  "$SOLANA" -u "$SOLANA_URL" balance "$1" --lamports 2>/dev/null | awk '{print $1}'
}

# After a reorg the enforcer wallet can still pick a coin that the reorg spent.
# One more block brings its view up to date.
deposit() {
  local out
  for _ in 1 2 3 4 5; do
    if out="$("$D" deposit $E "$@" 2>&1)"; then
      printf '%s\n' "$out"
      return 0
    fi
    case "$out" in
      *bad-txns-inputs-missingorspent*) "$D" mine $E --blocks 1 --address "$(new_address)" >/dev/null; sleep 3 ;;
      *) printf '%s\n' "$out" >&2; return 1 ;;
    esac
  done
  printf '%s\n' "$out" >&2
  return 1
}

start_peg

deposit --pubkey "$USER_PUBKEY" --sats "$DEPOSIT_SATS"
"$D" mine $E --blocks $((DEPOSIT_CONFIRMATIONS + 1)) --address "$COINBASE" >/dev/null

# The credit loop credits every eCash height in order, and the empty heights
# since the bridge start come first.
WANT_LAMPORTS=$((DEPOSIT_SATS * 10))
for _ in $(seq 1 120); do
  GOT="$("$SOLANA" -u "$SOLANA_URL" balance "$USER_PUBKEY" --lamports 2>/dev/null | awk '{print $1}')"
  [ "${GOT:-0}" = "$WANT_LAMPORTS" ] && break
  sleep 3
done
[ "${GOT:-0}" = "$WANT_LAMPORTS" ] || fail "the user holds ${GOT:-0} lamports, not $WANT_LAMPORTS"
echo "PASS: the peg in credited $GOT lamports for $DEPOSIT_SATS sats"
"$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID"

step "peg in while the daemon is down"
# The credits follow the cursor in the bridge config, so a restart misses no
# deposit.
kill "$PEG_PID"
wait "$PEG_PID" 2>/dev/null || true
DOWN_SATS=1500000
DOWN_PUBKEY="$("$D" seed-pubkey --mnemonic-file "$MNEMONIC_FILE" --account 1 | awk '{print $2}')"
deposit --pubkey "$DOWN_PUBKEY" --sats "$DOWN_SATS"
"$D" mine $E --blocks $((DEPOSIT_CONFIRMATIONS + 1)) --address "$COINBASE" >/dev/null
start_peg

WANT_DOWN=$((DOWN_SATS * 10))
for _ in $(seq 1 60); do
  GOT_DOWN="$(lamports_of "$DOWN_PUBKEY")"
  [ "${GOT_DOWN:-0}" = "$WANT_DOWN" ] && break
  sleep 3
done
[ "${GOT_DOWN:-0}" = "$WANT_DOWN" ] || fail "the downtime deposit gave ${GOT_DOWN:-0} lamports, not $WANT_DOWN"
echo "PASS: the restarted loop credited $GOT_DOWN lamports that landed while the daemon was down"

# The cursor never goes back, so the first deposit stays credited one time.
[ "$(lamports_of "$USER_PUBKEY")" = "$WANT_LAMPORTS" ] || fail "the first deposit got credited twice"
kill -0 "$PEG_PID" 2>/dev/null || fail "the daemon stopped after the restart"
echo "PASS: the first deposit stayed credited one time, and the daemon runs"

step "lose the wallet, then recover it from the seed phrase alone"
rm -f "$USER_KEY"
[ ! -f "$USER_KEY" ] || fail "the keypair file survived the delete"
"$D" seed-keypair --mnemonic-file "$MNEMONIC_FILE" --account 0 --out "$USER_KEY" >/dev/null
RECOVERED="$("$SOLANA_KEYGEN" pubkey "$USER_KEY")"
[ "$RECOVERED" = "$USER_PUBKEY" ] || fail "the recovery gives $RECOVERED, not $USER_PUBKEY"
RECOVERED_LAMPORTS="$("$SOLANA" -u "$SOLANA_URL" balance "$RECOVERED" --lamports | awk '{print $1}')"
[ "$RECOVERED_LAMPORTS" = "$WANT_LAMPORTS" ] || \
  fail "the recovered wallet holds $RECOVERED_LAMPORTS lamports, not $WANT_LAMPORTS"
echo "PASS: the seed phrase alone recovered the wallet and its $RECOVERED_LAMPORTS lamports"

step "peg out $WITHDRAW_SATS sats, signed by the recovered wallet"
PAYOUT_ADDRESS="$(new_address)"
echo "the payout goes to $PAYOUT_ADDRESS"

BEFORE_SATS="$(received "$PAYOUT_ADDRESS")"
"$D" withdraw --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  --user "$USER_KEY" --sats "$WITHDRAW_SATS" --fee-sats "$WITHDRAW_FEE_SATS" \
  --address "$PAYOUT_ADDRESS" --network regtest

step "mine until the bundle pays"
for _ in $(seq 1 20); do
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  sleep 4
  AFTER_SATS="$(received "$PAYOUT_ADDRESS")"
  [ "$AFTER_SATS" != "$BEFORE_SATS" ] && break
done
[ "$AFTER_SATS" != "$BEFORE_SATS" ] || fail "the payout never reached $PAYOUT_ADDRESS"
echo "PASS: the payout address went from $BEFORE_SATS BTC to $AFTER_SATS BTC"

step "the books must close"
"$D" status $E | grep -E "treasury|^slot $SLOT "
books_close
echo "PASS: both balance equations hold after the peg out"

step "BMM: a bid wins an eCash block, and a settle pays the payee of its pair"
bridge_value() {
  "$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
    | awk -v key="$1" '$0 ~ "^"key {print $NF}'
}
TREASURY_BEFORE="$(bridge_value "treasury lamports")"
echo "the treasury holds $TREASURY_BEFORE lamports of fees and reserve"
[ "$TREASURY_BEFORE" -gt 512 ] || fail "the treasury got no fees"

# A bid offers a part of the fee income, so the chain must earn fees. Ten
# transfers pay 100 lamports, which is 10 satoshis.
ORACLE_PUBKEY="$("$SOLANA_KEYGEN" pubkey "$ROOT/keys/oracle.json")"
# Each transfer takes another amount, so two of them under one blockhash are
# not the same tx.
earn_fees() {
  for index in $(seq 0 9); do
    "$SOLANA" -u "$SOLANA_URL" transfer --keypair "$USER_KEY" "$ORACLE_PUBKEY" \
      "0.00000100$index" --no-wait >/dev/null
  done
}

# A second validator with no vote proves the replay side of the pre-check. It
# marks a block dead if its own enforcer answers another way.
step "a second validator joins and replays"
GENESIS_HASH="$("$SOLANA" -u "$SOLANA_URL" genesis-hash)"
cp -R "$ROOT/keys" "$ROOT/keys2"
rm -f "$ROOT/keys2/validator-identity.json"
"$SOLANA_KEYGEN" new --no-bip39-passphrase --silent -o "$ROOT/keys2/validator-identity.json"
LEDGER="$ROOT/ledger2" KEYS="$ROOT/keys2" RPC_PORT="$((SOLANA_RPC_PORT + 10))" \
  GOSSIP_PORT="$((SOLANA_GOSSIP_PORT + 10))" XDP=0 NO_VOTE=1 \
  ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" \
  BMM_CONFIRMATIONS="$BMM_CONFIRMATIONS" ALLOW_PRIVATE_ADDR=1 \
  BIND_ADDRESS=127.0.0.1 NO_SNAPSHOT_FETCH=1 \
  ENTRYPOINT="127.0.0.1:$SOLANA_GOSSIP_PORT" \
  KNOWN_VALIDATOR="$("$SOLANA_KEYGEN" pubkey "$ROOT/keys/validator-identity.json")" \
  EXPECTED_GENESIS_HASH="$GENESIS_HASH" \
  nohup bash "$REPO/genesis/run-validator.sh" > "$ROOT/validator2.log" 2>&1 &
VALIDATOR2_PID=$!
PIDS+=("$VALIDATOR2_PID")
SECOND_URL="http://127.0.0.1:$((SOLANA_RPC_PORT + 10))"
SECOND_UP=no
for _ in $(seq 1 60); do
  if "$SOLANA" -u "$SECOND_URL" slot >/dev/null 2>&1; then
    SECOND_UP=yes
    break
  fi
  kill -0 "$VALIDATOR2_PID" 2>/dev/null || { tail -20 "$ROOT/validator2.log" >&2; fail "the second validator stopped"; }
  sleep 3
done
[ "$SECOND_UP" = yes ] || { tail -20 "$ROOT/validator2.log" >&2; fail "the second validator never answered"; }
SECOND_SLOT_BEFORE="$("$SOLANA" -u "$SECOND_URL" slot)"
echo "the second validator replays at slot $SECOND_SLOT_BEFORE"

start_bid_loop() {
  nohup "$D" bmm --enforcer-url "$ENFORCER_URL" --solana-rpc-url "$SOLANA_URL" \
    --slot "$SLOT" --program-id "$PROGRAM_ID" \
    --identity "$ROOT/keys/validator-identity.json" \
    --confirmations "$BMM_CONFIRMATIONS" --min-bid-sats 1 --interval-secs 1 \
    >> "$ROOT/bmm.log" 2>&1 &
  BMM_PID=$!
  PIDS+=("$BMM_PID")
}
start_bid_loop

# Each block gives the loop a new tip to bid on, and the loop publishes the
# pair of each bid. A win settles N + 1 blocks later, so the whole round takes
# about ten blocks.
PAID=no
for _ in $(seq 1 40); do
  kill -0 "$BMM_PID" 2>/dev/null || { tail -30 "$ROOT/bmm.log" >&2; fail "the BMM loop stopped"; }
  sleep 3
  earn_fees
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  if [ "$(bridge_value "bmm paid total")" -gt 0 ]; then
    PAID=yes
    break
  fi
done
[ "$PAID" = yes ] || { tail -40 "$ROOT/bmm.log" >&2; fail "no BMM win paid a winner"; }
grep -m1 "the loop settled its own win" "$ROOT/bmm.log" \
  || fail "the loop paid somebody else"
PAID_TOTAL="$(bridge_value "bmm paid total")"
NEXT_HEIGHT="$(bridge_value "bmm next height")"
[ "$NEXT_HEIGHT" -gt "$BMM_START" ] || fail "BMM settled no eCash height"
echo "PASS: a bid won, a settle paid $PAID_TOTAL lamports, and eCash height $NEXT_HEIGHT is next"

step "the second validator follows the settles"
SECOND_SLOT_AFTER="$("$SOLANA" -u "$SECOND_URL" slot)" || fail "the second validator stopped"
[ "$SECOND_SLOT_AFTER" -gt "$SECOND_SLOT_BEFORE" ] || \
  fail "the second validator stopped at slot $SECOND_SLOT_AFTER"
echo "PASS: the second validator replayed the settles, up to slot $SECOND_SLOT_AFTER"

step "BMM through the JSON-RPC: one round as drivechaind drives it"
# drivechaind pays the bids from a BitWindow wallet. With --no-bid the daemon
# gives the template, answers the checks, and settles.
kill "$BMM_PID"
wait "$BMM_PID" 2>/dev/null || true
# A new block drops the last bid of the old loop, so only the M8 below can win.
"$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
BMM_RPC_URL="http://127.0.0.1:$BMM_RPC_PORT"
NO_COLOR=1 nohup "$D" bmm --enforcer-url "$ENFORCER_URL" --solana-rpc-url "$SOLANA_URL" \
  --slot "$SLOT" --program-id "$PROGRAM_ID" \
  --identity "$ROOT/keys/validator-identity.json" \
  --confirmations "$BMM_CONFIRMATIONS" --min-bid-sats 1 --interval-secs 1 \
  --rpc-listen "127.0.0.1:$BMM_RPC_PORT" --no-bid \
  > "$ROOT/bmm-rpc.log" 2>&1 &
BMM_PID=$!
PIDS+=("$BMM_PID")
bmm_rpc() {
  curl -s --max-time 10 -X POST -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" "$BMM_RPC_URL"
}
# Prints one field of a JSON reply. A text prints bare, any other value as JSON.
field() {
  python3 -c 'import json,sys
v=json.load(sys.stdin)
for k in sys.argv[1:]:
    v=v[k]
print(v if isinstance(v,str) else json.dumps(v))' "$@"
}
TEMPLATE=""
for _ in $(seq 1 60); do
  kill -0 "$BMM_PID" 2>/dev/null || { tail -30 "$ROOT/bmm-rpc.log" >&2; fail "the BMM rpc stopped"; }
  TEMPLATE="$(bmm_rpc get_block_template '[]' || true)"
  printf '%s' "$TEMPLATE" | grep -q '"result"' && break
  sleep 2
done
printf '%s' "$TEMPLATE" | grep -q '"result"' || fail "get_block_template gave $TEMPLATE"
echo "get_block_template: $TEMPLATE"
CRITICAL="$(printf '%s' "$TEMPLATE" | field result critical_hash)"
BLOCK="$(printf '%s' "$TEMPLATE" | field result block)"
PREV="$(printf '%s' "$BLOCK" | field prev_main_hash)"
HEIGHT="$(printf '%s' "$BLOCK" | field height)"
FEES="$(printf '%s' "$TEMPLATE" | field result fees_sats)"
TIP_LINE="$("$D" ecash-tip $E)"
[ "$PREV" = "$(printf '%s' "$TIP_LINE" | awk '{print $2}')" ] || fail "prev_main_hash $PREV is not the eCash tip $TIP_LINE"
[ "$HEIGHT" = $(( $(printf '%s' "$TIP_LINE" | awk '{print $1}') + 1 )) ] || fail "the template height $HEIGHT is not the tip height + 1"
[ "$(bmm_rpc template_valid "[$BLOCK]" | field result)" = true ] || fail "the fresh template is not valid"

# drivechaind sends the same M8: h* = critical_hash, on top of prev_main_hash.
BID_SATS=$(( FEES > 1000 ? FEES : 1000 ))
# The enforcer wallet can still hold a coin that the last block spent, so a
# refused bid tries again on the same tip.
BID_SENT=no
for _ in $(seq 1 10); do
  if "$D" bid-bmm $E --solana-rpc-url "$SOLANA_URL" --commitment "$CRITICAL" \
      --sats "$BID_SATS" > "$ROOT/bid-rpc.log" 2>&1; then
    BID_SENT=yes
    break
  fi
  sleep 3
done
cat "$ROOT/bid-rpc.log"
[ "$BID_SENT" = yes ] || fail "the enforcer wallet did not send the M8"
grep -q "^commitment $CRITICAL\$" "$ROOT/bid-rpc.log" || fail "the M8 does not carry the critical hash"
grep -q "for eCash height $HEIGHT " "$ROOT/bid-rpc.log" || fail "the M8 is not for eCash height $HEIGHT"
"$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
WON="$(btc getblockhash "$HEIGHT")"
CONNECTED="$(bmm_rpc connect_block "[$BLOCK, \"$WON\"]")"
echo "connect_block: $CONNECTED"
[ "$(printf '%s' "$CONNECTED" | field result)" = true ] || fail "connect_block refused the block that won"
[ "$(bmm_rpc connect_block "[$BLOCK, \"$PREV\"]" | field result)" = false ] || fail "connect_block took another block"
INCLUSIONS="$(bmm_rpc get_bmm_inclusions "[\"$CRITICAL\"]")"
echo "get_bmm_inclusions: $INCLUSIONS"
[ "$(printf '%s' "$INCLUSIONS" | field result)" = "[\"$WON\"]" ] || fail "get_bmm_inclusions does not name $WON"

PAID_BEFORE_RPC="$(bridge_value "bmm paid total")"
SETTLED=no
for _ in $(seq 1 $((BMM_CONFIRMATIONS + 10))); do
  sleep 3
  earn_fees
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  if [ "$(bridge_value "bmm next height")" -gt "$HEIGHT" ]; then
    SETTLED=yes
    break
  fi
done
[ "$SETTLED" = yes ] || { tail -30 "$ROOT/bmm-rpc.log" >&2; fail "the loop did not settle eCash height $HEIGHT"; }
grep "the loop settled its own win" "$ROOT/bmm-rpc.log" | grep -q "height=$HEIGHT" \
  || { tail -30 "$ROOT/bmm-rpc.log" >&2; fail "the settle of eCash height $HEIGHT did not pay the payee of the template"; }
PAID_AFTER_RPC="$(bridge_value "bmm paid total")"
[ "$PAID_AFTER_RPC" -gt "$PAID_BEFORE_RPC" ] || fail "the settle paid nothing"
COUNT="$(bmm_rpc getblockcount '[]' | field result)"
[ "$COUNT" = $(( $(bridge_value "bmm next height") - 1 )) ] || fail "getblockcount gave $COUNT"
echo "PASS: one round through the JSON-RPC: the M8 for eCash height $HEIGHT won in $WON, and the settle paid $((PAID_AFTER_RPC - PAID_BEFORE_RPC)) lamports"
kill "$BMM_PID"
wait "$BMM_PID" 2>/dev/null || true
start_bid_loop

step "BMM: a settle for a block that is too new never lands"
TIP_HEIGHT="$(btc getblockcount)"
TIP_HASH="$(btc getblockhash "$TIP_HEIGHT")"
NEXT_BEFORE="$(bridge_value "bmm next height")"
SOLANA_BLOCK="$("$D" bmm-block --solana-rpc-url "$SOLANA_URL" | awk 'END {print $2}')"
if "$D" settle-bmm --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
    --identity "$ROOT/keys/oracle.json" --height "$TIP_HEIGHT" \
    --block-hash "$TIP_HASH" --solana-block "$SOLANA_BLOCK" > "$ROOT/settle-new.log" 2>&1; then
  fail "a settle for the tip landed, and it must wait for N + 1 blocks"
fi
grep -qi "restricted" "$ROOT/settle-new.log" || {
  cat "$ROOT/settle-new.log" >&2
  fail "the validator gave another error than a restricted program"
}
[ "$(bridge_value "bmm next height")" = "$NEXT_BEFORE" ] || fail "the height settled anyway"
echo "PASS: the leader holds back a settle until the block is N + 1 deep"

step "BMM: a settle for a stale block never lands"
# A block that a reorg drops is not on the active chain, so its settle fails
# whatever its depth.
STALE_HEIGHT=$(( $(bridge_value "bmm next height") ))
STALE_HASH="$(btc getblockhash "$STALE_HEIGHT")"
echo "eCash height $STALE_HEIGHT holds block $STALE_HASH"
btc invalidateblock "$STALE_HASH" >/dev/null
wait_for_enforcer
# A fresh address changes the coinbase, so the new blocks are not the same
# blocks that the node marked invalid.
COINBASE="$(new_address)"
# The new branch must be longer, so it takes over.
"$D" mine $E --blocks 10 --address "$COINBASE" >/dev/null
wait_for_enforcer
NEW_HASH="$(btc getblockhash "$STALE_HEIGHT")"
[ "$NEW_HASH" != "$STALE_HASH" ] || fail "the reorg did not change the block at $STALE_HEIGHT"
echo "the same height holds $NEW_HASH after the reorg"
sleep 3
if "$D" settle-bmm --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
    --identity "$ROOT/keys/oracle.json" --height "$STALE_HEIGHT" \
    --block-hash "$STALE_HASH" --solana-block "$SOLANA_BLOCK" > "$ROOT/settle-stale.log" 2>&1; then
  fail "a settle for a stale block landed"
fi
grep -qi "restricted" "$ROOT/settle-stale.log" || {
  cat "$ROOT/settle-stale.log" >&2
  fail "the validator gave another error than a restricted program"
}
echo "PASS: a settle for a block outside the active chain never lands"

step "BMM: the loop settles a commitment of the new chain past that height"
SETTLED=no
for _ in $(seq 1 20); do
  sleep 3
  earn_fees
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  if [ "$(bridge_value "bmm next height")" -gt "$STALE_HEIGHT" ]; then
    SETTLED=yes
    break
  fi
done
[ "$SETTLED" = yes ] || { tail -30 "$ROOT/bmm.log" >&2; fail "the loop did not settle past the reorg"; }
echo "PASS: the loop settled eCash height $STALE_HEIGHT from the new chain"

step "BMM: a reorg deeper than N over a settled height leaves the chain alive"
# The settles take heights about N + 1 below the tip, so a reorg of 9 blocks
# drops at least one settled eCash block. The migration below replays this
# ledger from the genesis through those settles.
for _ in $(seq 1 10); do
  [ "$(bridge_value "bmm next height")" -gt $(( $(btc getblockcount) - 8 )) ] && break
  sleep 3
  earn_fees
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
done
DEEP_TIP="$(btc getblockcount)"
DEEP_FROM=$((DEEP_TIP - 8))
PAID_BEFORE="$(bridge_value "bmm paid total")"
NEXT_AT_REORG="$(bridge_value "bmm next height")"
[ "$NEXT_AT_REORG" -gt "$DEEP_FROM" ] || fail "no settle took a height at or above $DEEP_FROM"
btc invalidateblock "$(btc getblockhash "$DEEP_FROM")" >/dev/null
wait_for_enforcer
COINBASE="$(new_address)"
"$D" mine $E --blocks 14 --address "$COINBASE" >/dev/null
wait_for_enforcer
echo "the chain reorged 9 blocks deep, more than N = $BMM_CONFIRMATIONS, and dropped the settled heights $DEEP_FROM to $((NEXT_AT_REORG - 1))"
ALIVE=no
for _ in $(seq 1 20); do
  sleep 3
  earn_fees
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  kill -0 "$BMM_PID" 2>/dev/null || { tail -30 "$ROOT/bmm.log" >&2; fail "the BMM loop stopped after the deep reorg"; }
  if [ "$(bridge_value "bmm next height")" -gt "$NEXT_AT_REORG" ]; then
    ALIVE=yes
    break
  fi
done
[ "$ALIVE" = yes ] || { tail -30 "$ROOT/bmm.log" >&2; fail "the chain stopped after a deep reorg"; }
[ "$(bridge_value "bmm paid total")" -ge "$PAID_BEFORE" ] || fail "the treasury paid a winner twice"
SECOND_SLOT_DEEP="$("$SOLANA" -u "$SECOND_URL" slot)" || fail "the second validator stopped"
[ "$SECOND_SLOT_DEEP" -gt "$SECOND_SLOT_AFTER" ] || fail "the second validator stopped after the reorg"
echo "PASS: both validators run after a reorg deeper than N"

step "the books close at the end, and the supply never changed"
books_close
SUPPLY_END="$(supply)"
[ "${SUPPLY_END% *}" = "${SUPPLY_START% *}" ] || \
  fail "the supply went from ${SUPPLY_START% *} at slot ${SUPPLY_START#* } to ${SUPPLY_END% *} at slot ${SUPPLY_END#* }"
echo "PASS: the supply stayed at ${SUPPLY_END% *} lamports from slot ${SUPPLY_START#* } to slot ${SUPPLY_END#* }"

# ---------------------------------------------------------------------------
step "migration: an open withdrawal, then the chain stops"
kill "$BMM_PID" "$PEG_PID" "$VALIDATOR2_PID" 2>/dev/null || true
wait "$BMM_PID" "$PEG_PID" "$VALIDATOR2_PID" 2>/dev/null || true
OPEN_ADDRESS="$(new_address)"
"$D" withdraw --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  --user "$USER_KEY" --sats 1000000 --fee-sats "$WITHDRAW_FEE_SATS" \
  --address "$OPEN_ADDRESS" --network regtest
books_close
OPEN_INDEX=$(( $(bridge_value "withdrawals") - 1 ))
OPEN_RECORD="$(rpc getProgramAccounts "[\"$PROGRAM_ID\",{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}]" \
  | python3 -c 'import json,sys; [print(a["pubkey"], a["account"]["lamports"], a["account"]["data"][0]) for a in json.load(sys.stdin)["result"] if a["account"]["space"] != 187]')"
[ "$(printf '%s\n' "$OPEN_RECORD" | grep -c .)" = 1 ] || fail "the chain holds these withdrawal records: $OPEN_RECORD"
# A halted node casts no vote, so its `confirmed` stays behind. The cursor
# reads `processed`, as the migration does.
cursor() {
  "$D" bridge-state --solana-rpc-url "$1" --program-id "$PROGRAM_ID" --processed \
    | grep -E "^(pegged lamports|deposit high water|withdrawals|credited height|credited block|credit index|stranded lamports)"
}
bmm_counters() {
  "$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" --processed \
    | grep -E "^(bmm paid total|bmm next height)"
}
CURSOR_BEFORE="$(cursor "$SOLANA_URL")"
DOWN_BEFORE="$(lamports_of "$DOWN_PUBKEY")"
USER_BEFORE="$(lamports_of "$USER_PUBKEY")"
echo "withdrawal $OPEN_INDEX stays open; the user holds $USER_BEFORE lamports"
BMM_BEFORE="$(bmm_counters)"
OLD_SLOT="$("$SOLANA" -u "$SOLANA_URL" slot --commitment processed)"
kill "$VALIDATOR_PID"
wait "$VALIDATOR_PID" 2>/dev/null || true

step "migration: the halted chain goes into a new genesis"
LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" RPC_PORT="$SOLANA_RPC_PORT" \
  GOSSIP_PORT="$SOLANA_GOSSIP_PORT" XDP=0 HALT=1 \
  ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" \
  BMM_CONFIRMATIONS="$BMM_CONFIRMATIONS" ALLOW_PRIVATE_ADDR=1 \
  nohup bash "$REPO/genesis/run-validator.sh" > "$ROOT/validator-halted.log" 2>&1 &
HALTED_PID=$!
PIDS+=("$HALTED_PID")
# The node replays its ledger first. It then stands still at the last slot,
# at or after the slot where the old validator stopped.
HALT_SLOT=0
for _ in $(seq 1 120); do
  kill -0 "$HALTED_PID" 2>/dev/null || fail "the halted validator stopped"
  HALT_SLOT="$("$SOLANA" -u "$SOLANA_URL" slot --commitment processed 2>/dev/null || echo 0)"
  [ "$HALT_SLOT" -ge "$OLD_SLOT" ] && break
  sleep 2
done
[ "$HALT_SLOT" -ge "$OLD_SLOT" ] || fail "the halted node replayed only to slot $HALT_SLOT, not $OLD_SLOT"
sleep 10
[ "$("$SOLANA" -u "$SOLANA_URL" slot --commitment processed)" = "$HALT_SLOT" ] || fail "the halted chain still makes blocks"
echo "the old validator stopped at slot $OLD_SLOT, and the halted node stands still at slot $HALT_SLOT"
[ "$(cursor "$SOLANA_URL")" = "$CURSOR_BEFORE" ] || fail "the halted node shows another cursor"
[ "$(bmm_counters)" = "$BMM_BEFORE" ] || fail "the halted node shows other BMM counters"
grep -q "takes the answer of its first run" "$ROOT/validator-halted.log" \
  || fail "the halted node replayed no settle with the answer of its first run"
echo "PASS: the halted node replayed from the genesis through the settles of the dropped eCash blocks"
OLD_RPC="$SOLANA_URL" LEDGER="$ROOT/ledger-new" KEYS="$ROOT/keys" STATE="$ROOT/state.json" \
  ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" NETWORK=regtest DAEMON="$D" \
  DEPOSIT_CONFIRMATIONS="$DEPOSIT_CONFIRMATIONS" \
  bash "$REPO/scripts/migrate-genesis.sh" | tee "$ROOT/migrate.log"
kill "$HALTED_PID"
wait "$HALTED_PID" 2>/dev/null || true

step "migration: the new chain starts"
LEDGER="$ROOT/ledger-new" KEYS="$ROOT/keys" RPC_PORT="$SOLANA_RPC_PORT" \
  GOSSIP_PORT="$SOLANA_GOSSIP_PORT" XDP=0 \
  ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" \
  BMM_CONFIRMATIONS="$BMM_CONFIRMATIONS" ALLOW_PRIVATE_ADDR=1 \
  nohup bash "$REPO/genesis/run-validator.sh" > "$ROOT/validator-new.log" 2>&1 &
VALIDATOR_PID=$!
PIDS+=("$VALIDATOR_PID")
HEALTHY=no
for _ in $(seq 1 90); do
  if rpc getHealth '[]' 2>/dev/null | grep -q '"result":"ok"'; then
    HEALTHY=yes
    break
  fi
  kill -0 "$VALIDATOR_PID" 2>/dev/null || fail "the new validator stopped"
  sleep 2
done
[ "$HEALTHY" = yes ] || fail "the new chain did not become healthy"
NEW_GENESIS="$("$SOLANA" -u "$SOLANA_URL" genesis-hash)"
[ "$NEW_GENESIS" != "$GENESIS_HASH" ] || fail "the new chain has the old genesis"
echo "the new chain $NEW_GENESIS runs at slot $("$SOLANA" -u "$SOLANA_URL" slot)"

[ "$(cursor "$SOLANA_URL")" = "$CURSOR_BEFORE" ] || {
  printf 'before:\n%s\nafter:\n%s\n' "$CURSOR_BEFORE" "$(cursor "$SOLANA_URL")" >&2
  fail "the cursor did not carry over"
}
echo "PASS: the cursor carried over:"
printf '%s\n' "$CURSOR_BEFORE"
[ "$(lamports_of "$USER_PUBKEY")" = "$USER_BEFORE" ] || fail "the user balance did not carry over"
[ "$(lamports_of "$DOWN_PUBKEY")" = "$DOWN_BEFORE" ] || fail "the second balance did not carry over"
echo "PASS: the balances carried over: $USER_BEFORE and $DOWN_BEFORE lamports"
NEW_RECORD="$(rpc getProgramAccounts "[\"$PROGRAM_ID\",{\"encoding\":\"base64\",\"commitment\":\"confirmed\"}]" \
  | python3 -c 'import json,sys; [print(a["pubkey"], a["account"]["lamports"], a["account"]["data"][0]) for a in json.load(sys.stdin)["result"] if a["account"]["space"] != 187]')"
[ "$NEW_RECORD" = "$OPEN_RECORD" ] || fail "the open withdrawal record did not carry over"
echo "PASS: open withdrawal $OPEN_INDEX carried over byte for byte"
books_close
echo "PASS: the books close on the new chain"

step "migration: the new chain credits only new deposits, and pays the open withdrawal"
BEFORE_OPEN="$(received "$OPEN_ADDRESS")"
PEGGED_BEFORE="$(bridge_value "pegged lamports")"
start_peg
NEW_SATS=700000
NEW_PUBKEY="$("$D" seed-pubkey --mnemonic-file "$MNEMONIC_FILE" --account 2 | awk '{print $2}')"
deposit --pubkey "$NEW_PUBKEY" --sats "$NEW_SATS"
"$D" mine $E --blocks $((DEPOSIT_CONFIRMATIONS + 1)) --address "$COINBASE" >/dev/null
for _ in $(seq 1 60); do
  [ "$(lamports_of "$NEW_PUBKEY")" = $((NEW_SATS * 10)) ] && break
  sleep 3
done
[ "$(lamports_of "$NEW_PUBKEY")" = $((NEW_SATS * 10)) ] || fail "the new chain did not credit the new deposit"
# The peg can close the open record in these blocks, and the user then gets
# its rent back. So the pegged count proves that no old deposit came again.
[ "$(bridge_value "pegged lamports")" = $((PEGGED_BEFORE + NEW_SATS * 10)) ] || \
  fail "the pegged count went from $PEGGED_BEFORE to $(bridge_value "pegged lamports")"
[ "$(lamports_of "$DOWN_PUBKEY")" = "$DOWN_BEFORE" ] || fail "the new chain credited an old deposit again"
echo "PASS: the new chain credited the new deposit, and no old deposit again"
for _ in $(seq 1 30); do
  [ "$(received "$OPEN_ADDRESS")" != "$BEFORE_OPEN" ] && break
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  sleep 4
done
[ "$(received "$OPEN_ADDRESS")" != "$BEFORE_OPEN" ] || fail "the open withdrawal was never paid"
books_close
echo "PASS: eCash paid the migrated withdrawal, and the books close"

echo
echo "PASS: the peg, BMM, BMM through the JSON-RPC, the reorg cases, and the migration all work, and the books close."
