#!/usr/bin/env bash
# Starts the one validator of the SOL drivechain against the built genesis.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEDGER="${LEDGER:-$REPO/genesis/ledger}"
KEYS="${KEYS:-$REPO/keys}"
RPC_PORT="${RPC_PORT:-8899}"
GOSSIP_PORT="${GOSSIP_PORT:-8001}"
# The validator advertises the address that it binds. A first validator has no
# entrypoint, so it cannot learn its own address: its operator sets
# BIND_ADDRESS to the address that the world sees. A loopback bind also stops
# its own votes from landing. Only the RPC stays on 127.0.0.1, behind a
# reverse proxy.
BIND_ADDRESS="${BIND_ADDRESS:-0.0.0.0}"
# A joiner takes the genesis and a snapshot from the RPC of another node, so a
# chain that anyone may join answers the RPC from outside. The reverse proxy
# still serves the friendly URL from 127.0.0.1.
RPC_BIND_ADDRESS="${RPC_BIND_ADDRESS:-127.0.0.1}"

AGAVE_VALIDATOR="${AGAVE_VALIDATOR:-$(command -v agave-validator || true)}"
SOLANA_KEYGEN="${SOLANA_KEYGEN:-$(command -v solana-keygen || true)}"
if [ -z "$AGAVE_VALIDATOR" ] || [ ! -x "$AGAVE_VALIDATOR" ]; then
  echo "error: agave-validator is missing. No Agave CLI release carries it." >&2
  echo "Build it from an agave checkout, then set AGAVE_VALIDATOR." >&2
  exit 1
fi

# The validator raises this limit itself, and it stops when it cannot. A
# process never raises its own hard limit, so the hard limit must already hold
# this number before the script runs.
NOFILE="${NOFILE:-1000000}"
# One host can hold only one XDP transmit socket per interface queue, so a
# second validator on the same box falls back to UDP.
NO_XDP=""
if [ "${XDP:-1}" = "0" ]; then
  NO_XDP="--no-xdp"
fi

# A second validator joins through these. ENTRYPOINT and KNOWN_VALIDATOR each
# take a space separated list. The first validator of the chain sets none of
# them, because it has nobody to join.
#
# EXPECTED_GENESIS_HASH stops a joiner from following another chain that
# answers on the same address. Set it on every joiner.
JOIN_ARGS=()
for entry in ${ENTRYPOINT:-}; do
  JOIN_ARGS+=(--entrypoint "$entry")
done
for known in ${KNOWN_VALIDATOR:-}; do
  JOIN_ARGS+=(--known-validator "$known")
done
# Each validator reads eCash from its own enforcer, or it cannot check a block
# that settles a BMM height.
if [ -z "${ENFORCER_URL:-}" ] || [ -z "${SIDECHAIN_SLOT:-}" ]; then
  echo "error: set ENFORCER_URL and SIDECHAIN_SLOT for the local eCash enforcer." >&2
  exit 1
fi
if [ -n "${EXPECTED_GENESIS_HASH:-}" ]; then
  JOIN_ARGS+=(--expected-genesis-hash "$EXPECTED_GENESIS_HASH")
fi
if [ ${#JOIN_ARGS[@]} -gt 0 ] && [ -z "${EXPECTED_GENESIS_HASH:-}" ]; then
  echo "error: a joiner must set EXPECTED_GENESIS_HASH." >&2
  echo "Read it from the chain: solana -u <rpc> genesis-hash" >&2
  exit 1
fi
HARD_NOFILE="$(ulimit -Hn)"
if [ "$HARD_NOFILE" != "unlimited" ] && [ "$HARD_NOFILE" -lt "$NOFILE" ]; then
  echo "error: the hard open file limit is $HARD_NOFILE, and the validator asks for $NOFILE." >&2
  echo "A process cannot raise its own hard limit. Raise it first, then run this again:" >&2
  echo "  sudo prlimit --pid \$\$ --nofile=$NOFILE:$NOFILE" >&2
  exit 1
fi
ulimit -n "$NOFILE"

if [ ! -d "$LEDGER" ]; then
  echo "error: no ledger at $LEDGER. Run genesis/build-genesis.sh first." >&2
  exit 1
fi

# A deposit waits 100 eCash blocks, about 17 hours. A restart after a reorg
# that deep starts from a snapshot at or before the lost slot, so the node
# keeps snapshots over that whole window. Ten full snapshots, 25000 slots
# apart, cover about 28 hours.
SNAPSHOT_ARGS=(
  --full-snapshot-interval-slots "${FULL_SNAPSHOT_INTERVAL_SLOTS:-25000}"
  --maximum-full-snapshots-to-retain "${FULL_SNAPSHOTS_TO_RETAIN:-10}"
)

# Gossip between nodes on one host uses loopback, which the validator takes
# only with this flag.
EXTRA_ARGS=()
# Two validators on one host must take different ports. The default range is
# 8000 to 8020, and the second node takes another one.
if [ -n "${DYNAMIC_PORT_RANGE:-}" ]; then
  EXTRA_ARGS+=(--dynamic-port-range "$DYNAMIC_PORT_RANGE")
fi
if [ -n "${ALLOW_PRIVATE_ADDR:-}" ]; then
  EXTRA_ARGS+=(--allow-private-addr)
fi
# A node with the genesis ledger replays every block, so it takes no snapshot.
if [ -n "${NO_SNAPSHOT_FETCH:-}" ]; then
  EXTRA_ARGS+=(--no-snapshot-fetch --no-genesis-fetch)
fi

# A node with NO_VOTE only replays. It carries no vote account.
VOTE_ARGS=(
  --vote-account "$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-vote.json")"
  --authorized-voter "$KEYS/validator-identity.json"
)
if [ -n "${NO_VOTE:-}" ]; then
  VOTE_ARGS=(--no-voting)
fi

# The default filter drops every message of the BMM crates.
export RUST_LOG="${RUST_LOG:-solana=info,sol_drivechain_bmm=debug,solana_runtime::bank::bmm=debug}"

exec "$AGAVE_VALIDATOR" \
  --ledger "$LEDGER" \
  --identity "$KEYS/validator-identity.json" \
  "${VOTE_ARGS[@]}" \
  "${SNAPSHOT_ARGS[@]}" \
  ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
  --rpc-port "$RPC_PORT" \
  --gossip-port "$GOSSIP_PORT" \
  --no-wait-for-vote-to-start-leader \
  $NO_XDP \
  ${JOIN_ARGS[@]+"${JOIN_ARGS[@]}"} \
  --no-poh-speed-test \
  --bmm-enforcer-url "$ENFORCER_URL" \
  --bmm-sidechain-slot "$SIDECHAIN_SLOT" \
  --bmm-confirmations "${BMM_CONFIRMATIONS:-6}" \
  --no-os-network-limits-test \
  --full-rpc-api \
  --bind-address "$BIND_ADDRESS" \
  --rpc-bind-address "$RPC_BIND_ADDRESS" \
  --log -
