#!/usr/bin/env bash
# Starts the one validator of the SOL drivechain against the built genesis.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEDGER="${LEDGER:-$REPO/genesis/ledger}"
KEYS="${KEYS:-$REPO/keys}"
RPC_PORT="${RPC_PORT:-8899}"
GOSSIP_PORT="${GOSSIP_PORT:-8001}"
# The validator sends its own votes through its advertised address, and a
# loopback bind stops those votes from landing. So the ports stay on 0.0.0.0,
# and genesis/close-validator-ports.sh drops outside traffic to them instead.
# Only the RPC stays on 127.0.0.1, behind a reverse proxy.
BIND_ADDRESS="${BIND_ADDRESS:-0.0.0.0}"

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

exec "$AGAVE_VALIDATOR" \
  --ledger "$LEDGER" \
  --identity "$KEYS/validator-identity.json" \
  --vote-account "$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-vote.json")" \
  --authorized-voter "$KEYS/validator-identity.json" \
  --rpc-port "$RPC_PORT" \
  --gossip-port "$GOSSIP_PORT" \
  --no-wait-for-vote-to-start-leader \
  $NO_XDP \
  ${JOIN_ARGS[@]+"${JOIN_ARGS[@]}"} \
  --no-poh-speed-test \
  --no-os-network-limits-test \
  --full-rpc-api \
  --bind-address "$BIND_ADDRESS" \
  --rpc-bind-address 127.0.0.1 \
  --log -
