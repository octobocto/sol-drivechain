#!/usr/bin/env bash
# Moves a halted chain into a new genesis. The new genesis holds the balance
# of each system account, each open withdrawal record, and the deposit cursor
# of the old chain. The new chain credits only the deposits after the cursor,
# so no deposit gets credited two times.
#
# Before this script:
#
# 1. Stop each sender of transactions: the public RPC proxy, the peg, the BMM
#    loop, and the faucet.
# 2. Stop the validator. Back up its ledger and its keys.
# 3. Start the validator again on the same ledger with HALT=1. It replays the
#    ledger and then makes no block, so the chain stands still at one slot.
#
# Then run this script with OLD_RPC set to the RPC of the halted node. It
# writes the new ledger at LEDGER, which must not exist yet. The new chain
# keeps the D and the lag of the old one.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OLD_RPC="${OLD_RPC:-}"
LEDGER="${LEDGER:-}"
KEYS="${KEYS:-$REPO/keys}"
STATE="${STATE:-}"
NETWORK="${NETWORK:-}"
ENFORCER_URL="${ENFORCER_URL:-http://127.0.0.1:50051}"
SIDECHAIN_SLOT="${SIDECHAIN_SLOT:-8}"
DAEMON="${DAEMON:-$REPO/daemon/target/release/sol-drivechain-daemon}"

fail() { echo "error: $1" >&2; exit 1; }

[ -n "$OLD_RPC" ] || fail "set OLD_RPC to the RPC of the halted validator"
[ -n "$NETWORK" ] || fail "set NETWORK to the network of the enforcer"
[ -n "$LEDGER" ] || fail "set LEDGER to the directory of the new ledger"
[ ! -e "$LEDGER" ] || fail "$LEDGER exists. Pick a new directory, and keep the old ledger"
[ -x "$DAEMON" ] || fail "the daemon is not at $DAEMON"
STATE="${STATE:-$(dirname "$LEDGER")/migration-state.json}"
PROGRAM_ID="$(cat "$KEYS/bridge-program.pubkey")"

echo "== the state of the halted chain"
OWNERLESS_ARGS=()
if [ -n "${ACCEPT_OWNERLESS_LAMPORTS:-}" ]; then
  OWNERLESS_ARGS=(--accept-ownerless-lamports "$ACCEPT_OWNERLESS_LAMPORTS")
fi
"$DAEMON" migrate-state --solana-rpc-url "$OLD_RPC" --program-id "$PROGRAM_ID" \
  --out "$STATE" ${OWNERLESS_ARGS[@]+"${OWNERLESS_ARGS[@]}"}

# Prints one value of the old bridge config. A halted node confirms nothing.
old_config() {
  "$DAEMON" bridge-state --solana-rpc-url "$OLD_RPC" --program-id "$PROGRAM_ID" --processed \
    | awk -v key="$1" 'index($0, key " ") == 1 {print $NF}'
}

carry() {
  local name="$1" old="$2" set="${!1:-}"
  [[ "$old" =~ ^[1-9][0-9]*$ ]] || fail "the old config gives no $name"
  [ -z "$set" ] || [ "$set" = "$old" ] \
    || fail "$name is $set, and the old chain uses $old. Leave $name unset"
  printf -v "$name" '%s' "$old"
}
carry DEPOSIT_CONFIRMATIONS "$(old_config "deposit confirmations")"
carry DEPOSIT_LAG "$(old_config "deposit lag")"
echo "the new chain keeps D = $DEPOSIT_CONFIRMATIONS and lag = $DEPOSIT_LAG"

echo "== the new genesis"
TIP="$("$DAEMON" ecash-tip --network "$NETWORK" --enforcer-url "$ENFORCER_URL" \
  --slot "$SIDECHAIN_SLOT" | awk '{print $1}')"
echo "BMM on the new chain settles from eCash height $TIP"
STATE="$STATE" ECASH_START_HEIGHT="$TIP" LEDGER="$LEDGER" KEYS="$KEYS" DAEMON="$DAEMON" \
  NETWORK="$NETWORK" DEPOSIT_CONFIRMATIONS="${DEPOSIT_CONFIRMATIONS:-}" \
  DEPOSIT_LAG="${DEPOSIT_LAG:-}" SIDECHAIN_SLOT="$SIDECHAIN_SLOT" \
  bash "$REPO/genesis/build-genesis.sh"

echo
echo "the new ledger is at $LEDGER, and the state of the old chain is in $STATE"
echo "next: stop the halted validator, and start the new one on $LEDGER"
