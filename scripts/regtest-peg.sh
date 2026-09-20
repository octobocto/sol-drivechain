#!/usr/bin/env bash
# Proves the whole peg on regtest: a slot claim, a peg in, and a peg out.
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

for binary in "$D" "$SOLANA" "$SOLANA_KEYGEN" "$SOLANA_GENESIS" "$AGAVE_VALIDATOR"; do
  [ -x "$binary" ] || fail "$binary is missing"
done

cleanup() {
  pkill -f "agave-validator --ledger $ROOT/ledger" 2>/dev/null || true
  pkill -f "sol-drivechain-daemon run --network regtest" 2>/dev/null || true
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
LEDGER="$ROOT/ledger" KEYS="$ROOT/keys" RPC_PORT="$SOLANA_RPC_PORT" \
  GOSSIP_PORT="$SOLANA_GOSSIP_PORT" XDP=0 \
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
"$D" initialize --solana-rpc-url "$SOLANA_URL" --program-id "$PROGRAM_ID" \
  --payer "$ROOT/keys/oracle.json" --oracle "$ROOT/keys/oracle.json"

step "claim sidechain slot $SLOT"
"$D" propose-slot $E >/dev/null
COINBASE="$("$D" wallet-address $E | head -1)"
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
PAYOUT_ADDRESS="$("$D" wallet-address $E | head -1)"
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

echo
echo "PASS: the peg in and the peg out both work, and the books close."
