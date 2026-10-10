#!/usr/bin/env bash
# Proves on regtest that BMM checkpoints select the Solana fork and move the
# Solana root, with two validators that vote.
#
# M holds 75 percent of the stake, and H holds 25 percent. M, H, and the eCash
# stack each run in their own network namespace. The test cuts the link
# between M and H to split the chain, and the host network stays as it is. The
# script makes namespaces, so it must run as root.
#
# Each bid commits h* = SHA-256(bank hash ‖ payee) for the newest block in the
# block record of one node, and the daemon publishes the pair to that node.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

ROOT="${ROOT:-$HOME/bmm-fork-regtest}"
SLOT="${SLOT:-8}"
K="${BMM_ROOT_DEPTH:-2}"
L="${BMM_FALLBACK_BLOCKS:-6}"
# N, the eCash blocks on top of a commitment before its settle.
N="${BMM_CONFIRMATIONS:-$K}"
MEMORY_MAX="${MEMORY_MAX:-3G}"
NS_E="${NS_PREFIX:-bmm}E"
NS_M="${NS_PREFIX:-bmm}M"
NS_H="${NS_PREFIX:-bmm}H"

# The eCash ports live inside the namespace of the eCash stack.
export RPC_PORT="${RPC_PORT:-21443}"
export P2P_PORT="${P2P_PORT:-21444}"
export ZMQ_PORT="${ZMQ_PORT:-21445}"
export GRPC_PORT="${GRPC_PORT:-21551}"
export ENFORCER_RPC_PORT="${ENFORCER_RPC_PORT:-21552}"
export GRPC_BIND=0.0.0.0
export ACCEPT_NONSTD="${ACCEPT_NONSTD:-1}"
SOLANA_RPC_PORT=28899

# M and H talk on 10.77.0.0/24. Each reads eCash on its own link.
M_IP=10.77.0.1
H_IP=10.77.0.2
E_FOR_M=10.77.1.1
M_FOR_E=10.77.1.2
E_FOR_H=10.77.2.1
H_FOR_E=10.77.2.2

D="${D:-$REPO/daemon/target/release/sol-drivechain-daemon}"
PROGRAM_SO="${PROGRAM_SO:-$REPO/target/deploy/sol_drivechain_bridge.so}"
PROGRAM_ID="$(cat "$REPO/keys/bridge-program.pubkey")"
LOADER=BPFLoader2111111111111111111111111111111111
# The rent reserve of an empty account under the default rent.
TREASURY_LAMPORTS=890880
SOLANA="${SOLANA:-$(command -v solana)}"
SOLANA_KEYGEN="${SOLANA_KEYGEN:-$(command -v solana-keygen)}"
SOLANA_GENESIS="${SOLANA_GENESIS:-$HOME/src/agave/target/release/solana-genesis}"
AGAVE_VALIDATOR="${AGAVE_VALIDATOR:-$HOME/src/agave/target/release/agave-validator}"

step() { printf '\n== %s ==\n' "$1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }
pass() { echo "PASS: $1"; }

in_ns() {
  local ns=$1
  shift
  ip netns exec "$ns" "$@"
}
btc() { in_ns "$NS_E" env ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" cli "$@"; }
d() { in_ns "$NS_E" "$D" "$@"; }
E=(--network regtest --enforcer-url "http://127.0.0.1:$GRPC_PORT" --slot "$SLOT")
mine() { d mine "${E[@]}" --blocks "$1" --address "$COINBASE" >/dev/null; }
tip() { btc getblockcount; }

ns_of() { if [ "$1" = M ]; then echo "$NS_M"; else echo "$NS_H"; fi; }
ip_of() { if [ "$1" = M ]; then echo "$M_IP"; else echo "$H_IP"; fi; }
enforcer_of() { if [ "$1" = M ]; then echo "$E_FOR_M"; else echo "$E_FOR_H"; fi; }
log_of() { if [ "$1" = M ]; then echo "$ROOT/m.log"; else echo "$ROOT/h.log"; fi; }
url_of() { echo "http://$(ip_of "$1"):$SOLANA_RPC_PORT"; }
sol() {
  local node=$1
  shift
  in_ns "$(ns_of "$node")" "$SOLANA" -u "$(url_of "$node")" "$@"
}
# The daemon in the namespace of a node, with the eCash enforcer on its link.
dn() {
  local node=$1 command=$2
  shift 2
  in_ns "$(ns_of "$node")" "$D" "$command" --network regtest \
    --enforcer-url "http://$(enforcer_of "$node"):$GRPC_PORT" --slot "$SLOT" "$@"
}
dn_solana() {
  local node=$1 command=$2
  shift 2
  in_ns "$(ns_of "$node")" "$D" "$command" --solana-rpc-url "$(url_of "$node")" "$@"
}
bridge_value() {
  dn_solana M bridge-state --program-id "$PROGRAM_ID" | awk -v key="$1" '$0 ~ "^"key {print $NF}'
}
lamports() { sol M balance "$1" --lamports | awk '{print $1}'; }
lines() { wc -l < "$(log_of "$1")"; }
# The log lines of a node after a line number.
log_since() { tail -n +"$(($2 + 1))" "$(log_of "$1")"; }

# Waits until the log of a node holds a line that matches a pattern, after a
# line number. Prints the line.
wait_log() {
  local node=$1 pattern=$2 since=$3 seconds=${4:-120} found
  for _ in $(seq 1 "$seconds"); do
    found="$(tail -n +"$((since + 1))" "$(log_of "$node")" | grep -aE -m1 "$pattern" || true)"
    if [ -n "$found" ]; then
      echo "$found"
      return 0
    fi
    sleep 1
  done
  fail "node $node logged no line that matches '$pattern' in $seconds seconds"
}

# The bank hash that a node froze for a slot, in base58.
frozen_hash() {
  grep -aE "bank frozen: $2 hash: " "$(log_of "$1")" | tail -1 \
    | sed -E 's/.*bank frozen: [0-9]+ hash: ([1-9A-HJ-NP-Za-km-z]+).*/\1/'
}

# The newest slot that a node froze, and its bank hash.
newest_frozen() {
  grep -aE "bank frozen: [0-9]+ hash: " "$(log_of "$1")" | tail -1 \
    | sed -E 's/.*bank frozen: ([0-9]+) hash: ([1-9A-HJ-NP-Za-km-z]+).*/\1 \2/'
}

# A confirmed slot of a node, and its bank hash.
confirmed_frozen() {
  local slot
  slot="$(sol "$1" slot --commitment confirmed)"
  echo "$slot $(frozen_hash "$1" "$slot")"
}

# The slot and the bank hash of the newest block in the block record of a
# node.
recorded() {
  dn_solana "$1" bmm-block | grep -E '^[0-9]+ [1-9A-HJ-NP-Za-km-z]+$'
}

# Waits until the block record of a node holds a block above a slot. A node
# records the last block of each 32 slots when it makes a block in a later
# stride, so a minority node takes a while.
wait_recorded_above() {
  local node=$1 slot=$2
  for _ in $(seq 1 240); do
    [ "$(recorded "$node" | awk '{print $1}')" -gt "$slot" ] && return 0
    sleep 0.5
  done
  fail "the block record of node $node holds no block above slot $slot"
}

# Bids for the newest block in the block record of a node, and publishes the
# pair to that node. The bid waits for a new record entry, so the block is a
# few slots old and above the stake root. Prints the slot and the bank hash.
# More arguments go to `bid-bmm`.
bid() {
  local node=$1 first out
  shift
  first="$(recorded "$node" | awk '{print $1}')"
  for _ in $(seq 1 120); do
    [ "$(recorded "$node" | awk '{print $1}')" != "$first" ] && break
    sleep 0.5
  done
  out="$(dn "$node" bid-bmm --solana-rpc-url "$(url_of "$node")" --payee "$PAYEE" \
    --sats 1000 "$@")"
  if [[ " $* " != *" --withhold "* ]]; then
    echo "$out" | grep -qE '^pair new$' || fail "node $node did not take the pair: $out"
  fi
  echo "$(echo "$out" | awk '/^slot /{print $2}') $(echo "$out" | awk '/^block /{print $2}')"
}

# A bid that the next eCash block takes. Prints the eCash height, the slot,
# and the bank hash.
checkpoint() {
  local bid_out
  bid_out="$(bid "$@")"
  mine 1
  echo "$(tip) $bid_out"
}

# The newest root that a node logged.
last_root() {
  grep -aE 'new root [0-9]+' "$(log_of "$1")" | tail -1 | sed -E 's/.*new root ([0-9]+).*/\1/'
}

# Sends fee-paying transfers, so the treasury grows.
pay_fees() {
  for _ in $(seq 1 "$1"); do
    sol M transfer --keypair "$ROOT/faucet.json" "$H_ID" 0.001 >/dev/null
  done
}

settle() {
  local height=$1 block=$2
  dn_solana M settle-bmm --program-id "$PROGRAM_ID" \
    --identity "$ROOT/keys-m/validator-identity.json" --height "$height" \
    --block-hash "$(btc getblockhash "$height")" --solana-block "$block" --payee "$PAYEE"
}

partition() { ip -n "$NS_M" link set vm0 down; }
heal() { ip -n "$NS_M" link set vm0 up; }

# True when the first base58 hash is lower than the second as bytes.
lower_hash() {
  python3 - "$1" "$2" <<'PY'
import sys
ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def decode(text):
    number = 0
    for char in text:
        number = number * 58 + ALPHABET.index(char)
    return number.to_bytes(32, "big")
sys.exit(0 if decode(sys.argv[1]) < decode(sys.argv[2]) else 1)
PY
}

stop_namespace() {
  local ns=$1 pids
  pids="$(ip netns pids "$ns" 2>/dev/null || true)"
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
  fi
}

cleanup() {
  if [ "${KEEP:-0}" = 1 ]; then
    echo "KEEP=1: the namespaces and the processes stay"
    return
  fi
  for ns in "$NS_M" "$NS_H" "$NS_E"; do
    stop_namespace "$ns"
  done
  sleep 5
  for ns in "$NS_M" "$NS_H" "$NS_E"; do
    pids="$(ip netns pids "$ns" 2>/dev/null || true)"
    if [ -n "$pids" ]; then
      # shellcheck disable=SC2086
      kill -9 $pids 2>/dev/null || true
    fi
    ip netns del "$ns" 2>/dev/null || true
  done
}

make_namespaces() {
  for ns in "$NS_E" "$NS_M" "$NS_H"; do
    ip netns add "$ns"
    ip -n "$ns" link set lo up
  done
  ip -n "$NS_M" link add vm0 type veth peer name vh0 netns "$NS_H"
  ip -n "$NS_E" link add e2m type veth peer name m2e netns "$NS_M"
  ip -n "$NS_E" link add e2h type veth peer name h2e netns "$NS_H"
  ip -n "$NS_M" addr add "$M_IP/24" dev vm0
  ip -n "$NS_H" addr add "$H_IP/24" dev vh0
  ip -n "$NS_E" addr add "$E_FOR_M/24" dev e2m
  ip -n "$NS_M" addr add "$M_FOR_E/24" dev m2e
  ip -n "$NS_E" addr add "$E_FOR_H/24" dev e2h
  ip -n "$NS_H" addr add "$H_FOR_E/24" dev h2e
  for pair in "$NS_M vm0" "$NS_H vh0" "$NS_E e2m" "$NS_M m2e" "$NS_E e2h" "$NS_H h2e"; do
    # shellcheck disable=SC2086
    set -- $pair
    ip -n "$1" link set "$2" up
  done
}

# Starts a validator in its namespace. An exit with code 75 is a BMM restart,
# and the loop starts the validator again, as systemd does on the host.
start_validator() {
  local node=$1 ns ip enforcer keys ledger
  ns="$(ns_of "$node")"
  if [ "$node" = M ]; then
    ip=$M_IP enforcer=$E_FOR_M keys=$ROOT/keys-m ledger=$ROOT/ledger-m
  else
    ip=$H_IP enforcer=$E_FOR_H keys=$ROOT/keys-h ledger=$ROOT/ledger-h
  fi
  local -a join=()
  if [ "$node" = H ]; then
    join=(ENTRYPOINT="$M_IP:8001" KNOWN_VALIDATOR="$M_ID" EXPECTED_GENESIS_HASH="$GENESIS_HASH")
  fi
  systemd-run --scope -q -p MemoryMax="$MEMORY_MAX" -p CPUWeight=10 \
    ip netns exec "$ns" env \
      LEDGER="$ledger" KEYS="$keys" RPC_PORT="$SOLANA_RPC_PORT" GOSSIP_PORT=8001 \
      XDP=0 BIND_ADDRESS="$ip" RPC_BIND_ADDRESS="$ip" ALLOW_PRIVATE_ADDR=1 NO_SNAPSHOT_FETCH=1 \
      ENFORCER_URL="http://$enforcer:$GRPC_PORT" SIDECHAIN_SLOT="$SLOT" \
      BMM_ROOT_DEPTH="$K" BMM_FALLBACK_BLOCKS="$L" BMM_CONFIRMATIONS="$N" \
      FULL_SNAPSHOT_INTERVAL_SLOTS=100 INCREMENTAL_SNAPSHOT_INTERVAL_SLOTS=50 \
      FULL_SNAPSHOTS_TO_RETAIN=100 \
      AGAVE_VALIDATOR="$AGAVE_VALIDATOR" SOLANA_KEYGEN="$SOLANA_KEYGEN" \
      ${join[@]+"${join[@]}"} \
      bash -c 'while true; do
          bash "$0"
          code=$?
          echo "== the validator stopped with exit code $code"
          [ "$code" = 75 ] || exit "$code"
        done' "$REPO/genesis/run-validator.sh" \
    >> "$(log_of "$node")" 2>&1 < /dev/null &
}

wait_rpc() {
  for _ in $(seq 1 120); do
    if sol "$1" slot >/dev/null 2>&1; then
      return 0
    fi
    if grep -aqE "stopped with exit code [0-9]*[^5]$|stopped with exit code [0-9]{3}" "$(log_of "$1")"; then
      tail -5 "$(log_of "$1")" >&2
      fail "node $1 stopped"
    fi
    sleep 2
  done
  fail "node $1 never answered its RPC"
}

for binary in "$D" "$SOLANA" "$SOLANA_KEYGEN" "$SOLANA_GENESIS" "$AGAVE_VALIDATOR"; do
  [ -x "$binary" ] || fail "$binary is missing"
done
[ -f "$PROGRAM_SO" ] || fail "the bridge program is not at $PROGRAM_SO"
[ "$(id -u)" = 0 ] || fail "the network namespaces need root"

trap cleanup EXIT
trap 'echo "FAIL: line $LINENO: $BASH_COMMAND" >&2' ERR

step "a clean slate"
cleanup
rm -rf "$ROOT"
mkdir -p "$ROOT/keys-m" "$ROOT/keys-h"
make_namespaces

step "the eCash node and the enforcer"
systemd-run --scope -q -p MemoryMax=2G -p CPUWeight=10 \
  ip netns exec "$NS_E" env ROOT="$ROOT/bitcoin-stack" bash "$HERE/regtest.sh" start
d propose-slot "${E[@]}" >/dev/null
COINBASE="$(d wallet-address "${E[@]}" 2>/dev/null | sed -n 1p)"
mine 1
d ack-slot "${E[@]}" >/dev/null
mine 6
d status "${E[@]}" | grep -E "^slot $SLOT " || fail "slot $SLOT did not activate"
# A bid spends a mature coinbase.
mine 101
sleep 3

step "a genesis with two staked validators"
for name in identity vote stake; do
  "$SOLANA_KEYGEN" new --no-bip39-passphrase --silent -o "$ROOT/keys-m/validator-$name.json"
  "$SOLANA_KEYGEN" new --no-bip39-passphrase --silent -o "$ROOT/keys-h/validator-$name.json"
done
"$SOLANA_KEYGEN" new --no-bip39-passphrase --silent -o "$ROOT/faucet.json"
"$SOLANA_KEYGEN" new --no-bip39-passphrase --silent -o "$ROOT/oracle.json"
M_ID="$("$SOLANA_KEYGEN" pubkey "$ROOT/keys-m/validator-identity.json")"
H_ID="$("$SOLANA_KEYGEN" pubkey "$ROOT/keys-h/validator-identity.json")"
PAYEE="$M_ID"
"$D" genesis --program-id "$PROGRAM_ID" \
  --oracle "$("$SOLANA_KEYGEN" pubkey "$ROOT/oracle.json")" \
  --treasury-lamports "$TREASURY_LAMPORTS" --out "$ROOT/primordial.yaml" >/dev/null
TREASURY="$("$D" derive --program-id "$PROGRAM_ID" | awk '/^treasury/ {print $2}')"
cat > "$ROOT/validators.yaml" <<EOF
validator_accounts:
  - balance_lamports: 1000000000000
    stake_lamports: 100000000000
    identity_account: $H_ID
    vote_account: $("$SOLANA_KEYGEN" pubkey "$ROOT/keys-h/validator-vote.json")
    stake_account: $("$SOLANA_KEYGEN" pubkey "$ROOT/keys-h/validator-stake.json")
EOF
"$SOLANA_GENESIS" --ledger "$ROOT/ledger-m" --cluster-type development \
  --bootstrap-validator "$M_ID" \
    "$("$SOLANA_KEYGEN" pubkey "$ROOT/keys-m/validator-vote.json")" \
    "$("$SOLANA_KEYGEN" pubkey "$ROOT/keys-m/validator-stake.json")" \
  --bootstrap-validator-lamports 1000000000000 \
  --bootstrap-validator-stake-lamports 300000000000 \
  --validator-accounts-file "$ROOT/validators.yaml" \
  --faucet-pubkey "$("$SOLANA_KEYGEN" pubkey "$ROOT/faucet.json")" \
  --faucet-lamports 1000000000000 --hashes-per-tick sleep \
  --primordial-accounts-file "$ROOT/primordial.yaml" \
  --bpf-program "$PROGRAM_ID" "$LOADER" "$PROGRAM_SO" >/dev/null
cp -R "$ROOT/ledger-m" "$ROOT/ledger-h"

step "M and H start and vote"
start_validator M
wait_rpc M
GENESIS_HASH="$(sol M genesis-hash)"
start_validator H
wait_rpc H
for _ in $(seq 1 120); do
  M_SLOT="$(sol M slot)"
  H_SLOT="$(sol H slot 2>/dev/null || echo 0)"
  [ "$H_SLOT" -gt 50 ] && [ $((M_SLOT - H_SLOT)) -lt 10 ] && break
  sleep 2
done
[ $((M_SLOT - H_SLOT)) -lt 10 ] || fail "H did not catch up: M at $M_SLOT, H at $H_SLOT"
sol M validators | grep -E "$M_ID|$H_ID" || fail "the validators are not active"
dn_solana M initialize --program-id "$PROGRAM_ID" --payer "$ROOT/oracle.json" \
  --oracle "$ROOT/oracle.json" --bmm-start-height "$(tip)" >/dev/null
pass "M at slot $M_SLOT and H at slot $H_SLOT; the bridge settles from eCash height $(bridge_value "bmm next height")"

# ---------------------------------------------------------------------------
step "1. normal: a checkpoint anchors, and the root follows it after K blocks"
MARK_M=$(lines M) MARK_H=$(lines H)
read -r H1 S1 S1_HASH < <(checkpoint M)
echo "eCash height $H1 commits slot $S1 ($S1_HASH)"
wait_log M "head Some\($S1\)" "$MARK_M" >/dev/null
wait_log H "head Some\($S1\)" "$MARK_H" >/dev/null
mine "$K"
wait_log M "moves the root from [0-9]+ to anchor $S1\$" "$MARK_M"
wait_log H "moves the root from [0-9]+ to anchor $S1\$" "$MARK_H"
CONFIRMED_BEFORE="$(sol M slot --commitment confirmed)"
sleep 20
CONFIRMED_AFTER="$(sol M slot --commitment confirmed)"
[ "$CONFIRMED_AFTER" -gt "$CONFIRMED_BEFORE" ] || fail "confirmed stayed at $CONFIRMED_BEFORE"
LAST_ROOT_M="$(last_root M)"
[ "$LAST_ROOT_M" = "$S1" ] || fail "M rooted slot $LAST_ROOT_M after the anchor $S1"
pass "both roots moved to anchor $S1; confirmed went from $CONFIRMED_BEFORE to $CONFIRMED_AFTER; the stake votes rooted nothing"

# ---------------------------------------------------------------------------
step "2. switch above the anchor: a branch with more checkpoints wins"
partition
SPLIT="$(sol M slot)"
echo "the link is down at slot $SPLIT"
sleep 20
read -r A2 _ < <(newest_frozen M)
wait_recorded_above H "$SPLIT"
read -r B2 B2_HASH < <(bid H)
[ "$B2" -gt "$SPLIT" ] || fail "the bid names slot $B2, which is not after the split"
heal
echo "the link is up; H bid for slot $B2 ($B2_HASH) on its own branch"
sleep 20
MARK_M=$(lines M) MARK_H=$(lines H)
mine 1
H2="$(tip)"
echo "eCash height $H2 commits slot $B2"
wait_log H "head Some\($B2\)" "$MARK_H" >/dev/null
wait_log M "head Some\($B2\)" "$MARK_M" 240 >/dev/null
wait_log M "resets the tower" "$MARK_M" 60
mine "$K"
wait_log M "moves the root from [0-9]+ to anchor $B2\$" "$MARK_M"
wait_log H "moves the root from [0-9]+ to anchor $B2\$" "$MARK_H"
sleep 10
read -r C2 C2_HASH < <(confirmed_frozen M)
[ "$(frozen_hash H "$C2")" = "$C2_HASH" ] || fail "M and H froze slot $C2 with other hashes"
grep -aE "BMM fork choice selects slot [0-9]+ over slot" "$(log_of M)" | tail -1
pass "M left branch A (slot $A2) for branch B at slot $B2, reset its tower, and froze slot $C2 as H did"

# ---------------------------------------------------------------------------
step "3. pending: a checkpoint for an absent block counts when the block arrives"
partition
SPLIT3="$(sol H slot)"
sleep 20
wait_recorded_above H "$SPLIT3"
MARK_M=$(lines M) MARK_H=$(lines H)
read -r H3 B3 _ < <(checkpoint H)
[ "$B3" -gt "$SPLIT3" ] || fail "the bid names slot $B3, which is not after the split"
echo "eCash height $H3 commits slot $B3, which only H holds, and only H has the pair"
wait_log H "head Some\($B3\)" "$MARK_H" >/dev/null
sleep 10
if tail -n +"$((MARK_M + 1))" "$(log_of M)" | grep -aqE "head Some\($B3\)"; then
  fail "M counted a checkpoint for a block that it does not hold"
fi
[ -z "$(frozen_hash M "$B3")" ] || fail "M holds slot $B3 before the link is up"
echo "M does not hold slot $B3, so the checkpoint is pending on M"
heal
wait_log M "head Some\($B3\)" "$MARK_M" 240
mine "$K"
wait_log M "moves the root from [0-9]+ to anchor $B3\$" "$MARK_M"
wait_log H "moves the root from [0-9]+ to anchor $B3\$" "$MARK_H"
pass "the checkpoint for slot $B3 stayed pending on M, and it counted when M replayed the block"

# ---------------------------------------------------------------------------
step "4. tie: equal counts select the lower bank hash"
partition
SPLIT4="$(sol M slot)"
sleep 20
wait_recorded_above M "$SPLIT4"
wait_recorded_above H "$SPLIT4"
MARK_M=$(lines M) MARK_H=$(lines H)
read -r _ A4 A4_HASH < <(checkpoint M)
read -r _ B4 B4_HASH < <(checkpoint H)
[ "$A4" -gt "$SPLIT4" ] && [ "$B4" -gt "$SPLIT4" ] || fail "a bid names a slot before the split"
if lower_hash "$A4_HASH" "$B4_HASH"; then
  WINNER=$A4
else
  WINNER=$B4
fi
echo "slot $A4 ($A4_HASH) on A and slot $B4 ($B4_HASH) on B each have one checkpoint; slot $WINNER has the lower hash"
heal
wait_log M "head Some\($WINNER\)" "$MARK_M" 240
wait_log H "head Some\($WINNER\)" "$MARK_H" 240
mine "$K"
wait_log M "moves the root from [0-9]+ to anchor $WINNER\$" "$MARK_M"
wait_log H "moves the root from [0-9]+ to anchor $WINNER\$" "$MARK_H"
ANCHOR4=$WINNER
pass "both nodes selected slot $WINNER, the lower bank hash, and rooted it"

# ---------------------------------------------------------------------------
step "5. fallback: stake roots after L blocks, then a restart joins the BMM branch"
partition
SPLIT5="$(sol M slot)"
MARK_M=$(lines M) MARK_H=$(lines H)
mine "$L"
wait_log M "fallback true" "$MARK_M" 60
wait_log H "fallback true" "$MARK_H" 60
for _ in $(seq 1 120); do
  ROOT_M="$(last_root M)"
  [ "$ROOT_M" -gt "$SPLIT5" ] && break
  sleep 1
done
[ "$ROOT_M" -gt "$SPLIT5" ] || fail "the stake votes did not root branch A on M"
echo "in the fallback, the stake votes rooted slot $ROOT_M on M, past the split at $SPLIT5"
wait_recorded_above H "$SPLIT5"
MARK_M=$(lines M) MARK_H=$(lines H)
read -r _ B5 _ < <(checkpoint H)
[ "$B5" -gt "$SPLIT5" ] || fail "the bid names slot $B5, which is not after the split"
mine "$K"
heal
wait_log M "select a branch that splits behind root" "$MARK_M" 60
wait_log M "the validator stopped with exit code 75" "$MARK_M" 60
wait_log M "BMM restart: the start loads a snapshot at or below slot $ANCHOR4" "$MARK_M" 120
wait_log M "head Some\($B5\)" "$MARK_M" 600
wait_log M "moves the root from [0-9]+ to anchor $B5\$" "$MARK_M" 120
wait_log H "moves the root from [0-9]+ to anchor $B5\$" "$MARK_H" 120
sleep 10
read -r C5 C5_HASH < <(confirmed_frozen H)
for _ in $(seq 1 30); do
  [ "$(frozen_hash M "$C5")" = "$C5_HASH" ] && break
  sleep 2
done
[ "$(frozen_hash M "$C5")" = "$C5_HASH" ] || fail "M and H froze slot $C5 with other hashes"
pass "M restarted from a snapshot at or below anchor $ANCHOR4, replayed branch B, and rooted anchor $B5"

# ---------------------------------------------------------------------------
step "6. restart from disk: M resumes on the BMM branch"
stop_namespace "$NS_M"
sleep 5
MARK_M=$(lines M)
start_validator M
wait_rpc M
wait_log M "BMM fork choice is on" "$MARK_M" 60
if tail -n +"$((MARK_M + 1))" "$(log_of M)" | grep -aq "found new cluster confirmed root"; then
  fail "M took a root from the stake votes at start"
fi
MARK_M=$(lines M) MARK_H=$(lines H)
read -r _ S6 _ < <(checkpoint H)
mine "$K"
wait_log M "moves the root from [0-9]+ to anchor $S6\$" "$MARK_M" 240
wait_log H "moves the root from [0-9]+ to anchor $S6\$" "$MARK_H"
pass "M started from its own ledger with no stake root, and it rooted anchor $S6 with H"

# ---------------------------------------------------------------------------
step "7. a commitment without a pair stops no root and causes no restart"
MARK_M=$(lines M) MARK_H=$(lines H)
mine "$L"
wait_log M "fallback true" "$MARK_M" 60
wait_log H "fallback true" "$MARK_H" 60
sleep 5
ROOT7="$(last_root M)"
MARK_M=$(lines M) MARK_H=$(lines H)
FAKE="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
d bid-bmm "${E[@]}" --commitment "$FAKE" --sats 1000 >/dev/null
mine 1
H7="$(tip)"
mine "$K"
wait_log M "the commitment at eCash height $H7 has no published pair" "$MARK_M" 60
wait_log H "the commitment at eCash height $H7 has no published pair" "$MARK_H" 60
sleep 20
for node in "M $MARK_M" "H $MARK_H"; do
  # shellcheck disable=SC2086
  set -- $node
  if log_since "$1" "$2" | grep -aE "fallback false|splits behind root|stopped with exit code"; then
    fail "the commitment without a pair changed the fork choice on node $1"
  fi
done
[ "$(last_root M)" -gt "$ROOT7" ] || fail "the stake votes stopped at root $ROOT7"
pass "the fake commitment at eCash height $H7 left the fallback on; the root went from $ROOT7 to $(last_root M)"

# ---------------------------------------------------------------------------
step "8. a withheld pair stays pending, and the next checkpoint takes its fees"
pay_fees 3
MARK_M=$(lines M) MARK_H=$(lines H)
read -r HW SW SW_HASH < <(checkpoint M --withhold)
TREASURY_W="$(lamports "$TREASURY")"
echo "eCash height $HW commits slot $SW, and the bidder keeps the pair; the treasury holds $TREASURY_W"
wait_log M "the commitment at eCash height $HW has no published pair" "$MARK_M" 60 >/dev/null
wait_log H "the commitment at eCash height $HW has no published pair" "$MARK_H" 60 >/dev/null
sleep 10
for node in "M $MARK_M" "H $MARK_H"; do
  # shellcheck disable=SC2086
  set -- $node
  if log_since "$1" "$2" | grep -aE "head Some\($SW\)|fallback false"; then
    fail "node $1 counted the commitment without a pair"
  fi
done
pay_fees 2
read -r HN SN SN_HASH < <(checkpoint M)
wait_log M "head Some\($SN\)" "$MARK_M" 60 >/dev/null
wait_log H "head Some\($SN\)" "$MARK_H" 60 >/dev/null
mine $((N + 1))
sleep 3
PAID_BEFORE="$(bridge_value "bmm paid total")"
for _ in $(seq 1 20); do
  settle "$HN" "$SN_HASH" > "$ROOT/settle-next.log" 2>&1 && break
  sleep 3
done
grep -q "settled" "$ROOT/settle-next.log" || { cat "$ROOT/settle-next.log" >&2; fail "the next checkpoint did not settle"; }
PAID=$(( $(bridge_value "bmm paid total") - PAID_BEFORE ))
[ "$PAID" -ge $((TREASURY_W - TREASURY_LAMPORTS)) ] || fail "the payee got $PAID, less than the fees before the withheld height"
[ "$(bridge_value "bmm next height")" = $((HN + 1)) ] || fail "the cursor is not past eCash height $HN"
echo "eCash height $HN paid $PAID lamports; the fees before the withheld height $HW were $((TREASURY_W - TREASURY_LAMPORTS))"
if settle "$HW" "$SW_HASH" > "$ROOT/settle-late.log" 2>&1; then
  fail "the withheld height settled after the cursor passed it"
fi
[ "$(bridge_value "bmm next height")" = $((HN + 1)) ] || fail "the late settle moved the cursor"
pass "the withheld pair stayed pending, the settle of height $HN took the rolled-over fees, and height $HW cannot settle later"

echo
echo "PASS: BMM checkpoints select the fork and move the root in all eight cases."
