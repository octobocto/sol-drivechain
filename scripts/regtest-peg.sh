#!/usr/bin/env bash
# Proves the whole peg on regtest: a slot claim, a peg in, a peg out, and a
# BMM win that pays the treasury to a validator.
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

cleanup() {
  pkill -f "agave-validator --ledger $ROOT/ledger" 2>/dev/null || true
  pkill -f "sol-drivechain-daemon run --network regtest" 2>/dev/null || true
  pkill -f "sol-drivechain-daemon bmm --enforcer-url $ENFORCER_URL" 2>/dev/null || true
  pkill -f "agave-validator --ledger $ROOT/ledger2" 2>/dev/null || true
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
LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" bash "$REPO/genesis/build-genesis.sh" >/dev/null
# A copy of the fresh ledger lets a second validator replay from the genesis,
# with no snapshot download.
cp -R "$ROOT/ledger" "$ROOT/ledger2"
LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" RPC_PORT="$SOLANA_RPC_PORT" \
  GOSSIP_PORT="$SOLANA_GOSSIP_PORT" XDP=0 \
  ENFORCER_URL="$ENFORCER_URL" SIDECHAIN_SLOT="$SLOT" \
  BMM_CONFIRMATIONS="$BMM_CONFIRMATIONS" ALLOW_PRIVATE_ADDR=1 \
  nohup bash "$REPO/genesis/run-validator.sh" > "$ROOT/validator.log" 2>&1 &
VALIDATOR_PID=$!
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

step "the bridge"
BMM_START="$(btc getblockcount)"
"$D" initialize --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  --payer "$ROOT/keys/oracle.json" --oracle "$ROOT/keys/oracle.json" \
  --bmm-start-height "$BMM_START"

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

nohup "$D" run --network regtest --enforcer-url "$ENFORCER_URL" \
  --solana-rpc-url "$SOLANA_URL" --slot "$SLOT" --program-id "$PROGRAM_ID" \
  --oracle "$ROOT/keys/oracle.json" --confirmations 1 --bundle-interval-secs 10 \
  > "$ROOT/peg.log" 2>&1 &
sleep 5

"$D" deposit $E --pubkey "$USER_PUBKEY" --sats "$DEPOSIT_SATS"
"$D" mine $E --blocks 2 --address "$COINBASE" >/dev/null

WANT_LAMPORTS=$((DEPOSIT_SATS * 10))
for _ in $(seq 1 30); do
  GOT="$("$SOLANA" -u "$SOLANA_URL" balance "$USER_PUBKEY" --lamports 2>/dev/null | awk '{print $1}')"
  [ "${GOT:-0}" = "$WANT_LAMPORTS" ] && break
  sleep 3
done
[ "${GOT:-0}" = "$WANT_LAMPORTS" ] || fail "the user holds ${GOT:-0} lamports, not $WANT_LAMPORTS"
echo "PASS: the peg in credited $GOT lamports for $DEPOSIT_SATS sats"
"$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID"

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
"$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID"
"$D" status $E | grep -E "treasury|^slot $SLOT "

PEGGED="$("$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  | awk '/^pegged lamports/ {print $3}')"
TREASURY="$("$D" status $E | awk '/^treasury/ {print $2}')"
[ -n "$PEGGED" ] || fail "the bridge reports no pegged lamport count"
[ -n "$TREASURY" ] || fail "the mainchain reports no treasury"

# The peg holds when the mainchain treasury matches the lamports that the
# bridge says it handed out, at ten lamports for each satoshi.
WANT=$((TREASURY * 10))
[ "$PEGGED" = "$WANT" ] || fail "the bridge says $PEGGED lamports, the treasury says $WANT"
echo "PASS: $PEGGED pegged lamports match $TREASURY treasury satoshis"

# The vault holds every lamport that no deposit handed out.
VAULT="$("$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  | awk '/^vault lamports/ {print $3}')"
VAULT_WANT=$((21000000 * 1000000000 - PEGGED))
[ "$VAULT" = "$VAULT_WANT" ] || fail "the vault holds $VAULT lamports, not $VAULT_WANT"
echo "PASS: the vault holds $VAULT lamports, the genesis total less the pegged count"

step "BMM: a bid wins an eCash block, and a settle pays the winner"
bridge_value() {
  "$D" bridge-state --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
    | awk -v key="$1" '$0 ~ "^"key {print $NF}'
}
TREASURY_BEFORE="$(bridge_value "treasury lamports")"
echo "the treasury holds $TREASURY_BEFORE lamports of fees and reserve"
[ "$TREASURY_BEFORE" -gt 1024 ] || fail "the treasury got no fees"

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

nohup "$D" bmm --enforcer-url "$ENFORCER_URL" --solana-rpc-url "$SOLANA_URL" \
  --slot "$SLOT" --program-id "$PROGRAM_ID" \
  --identity "$ROOT/keys/validator-identity.json" \
  --confirmations "$BMM_CONFIRMATIONS" --min-bid-sats 1 --interval-secs 1 \
  > "$ROOT/bmm.log" 2>&1 &
BMM_PID=$!

# Each block gives the loop a new tip to bid on. A win settles N + 1 blocks
# later, so the whole round takes about ten blocks.
PAID=no
for _ in $(seq 1 40); do
  kill -0 "$BMM_PID" 2>/dev/null || { tail -30 "$ROOT/bmm.log" >&2; fail "the BMM loop stopped"; }
  sleep 3
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

step "BMM: a settle for a block that is too new never lands"
TIP_HEIGHT="$(btc getblockcount)"
TIP_HASH="$(btc getblockhash "$TIP_HEIGHT")"
NEXT_BEFORE="$(bridge_value "bmm next height")"
if "$D" settle-bmm --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
    --identity "$ROOT/keys/oracle.json" --height "$TIP_HEIGHT" \
    --block-hash "$TIP_HASH" > "$ROOT/settle-new.log" 2>&1; then
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
    --block-hash "$STALE_HASH" > "$ROOT/settle-stale.log" 2>&1; then
  fail "a settle for a stale block landed"
fi
grep -qi "restricted" "$ROOT/settle-stale.log" || {
  cat "$ROOT/settle-stale.log" >&2
  fail "the validator gave another error than a restricted program"
}
echo "PASS: a settle for a block outside the active chain never lands"

step "BMM: the loop settles the new block at that height"
SETTLED=no
for _ in $(seq 1 20); do
  sleep 3
  "$D" mine $E --blocks 1 --address "$COINBASE" >/dev/null
  if [ "$(bridge_value "bmm next height")" -gt "$STALE_HEIGHT" ]; then
    SETTLED=yes
    break
  fi
done
[ "$SETTLED" = yes ] || { tail -30 "$ROOT/bmm.log" >&2; fail "the loop did not settle past the reorg"; }
echo "PASS: the loop settled eCash height $STALE_HEIGHT from the new chain"

step "BMM: a reorg deeper than N leaves the chain alive"
DEEP_TIP="$(btc getblockcount)"
DEEP_HASH="$(btc getblockhash $((DEEP_TIP - 8)))"
PAID_BEFORE="$(bridge_value "bmm paid total")"
NEXT_AT_REORG="$(bridge_value "bmm next height")"
btc invalidateblock "$DEEP_HASH" >/dev/null
wait_for_enforcer
COINBASE="$(new_address)"
"$D" mine $E --blocks 14 --address "$COINBASE" >/dev/null
wait_for_enforcer
echo "the chain reorged 9 blocks deep, under N + 1 = $((BMM_CONFIRMATIONS + 1))"
ALIVE=no
for _ in $(seq 1 20); do
  sleep 3
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

echo
echo "PASS: the peg, BMM, and the reorg cases all work, and the books close."
