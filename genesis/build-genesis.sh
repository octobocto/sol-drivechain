#!/usr/bin/env bash
# Builds the genesis ledger of the SOL drivechain.
#
# Every Solana constant that the peg changes comes from a flag here. The
# validator stays stock, so an upstream bump is a binary swap.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEDGER="${LEDGER:-$REPO/genesis/ledger}"
KEYS="${KEYS:-$REPO/keys}"
PROGRAM_SO="${PROGRAM_SO:-$REPO/target/deploy/sol_drivechain_bridge.so}"
DAEMON="${DAEMON:-}"
if [ -z "$DAEMON" ]; then
  for candidate in "$REPO/daemon/target/release/sol-drivechain-daemon" \
                   "$REPO/daemon/target/debug/sol-drivechain-daemon"; do
    if [ -x "$candidate" ]; then DAEMON="$candidate"; break; fi
  done
fi
# `solana-genesis` is absent from every Agave CLI release, so a host builds it
# from an agave checkout and names it here.
SOLANA_GENESIS="${SOLANA_GENESIS:-$(command -v solana-genesis || true)}"
SOLANA_KEYGEN="${SOLANA_KEYGEN:-$(command -v solana-keygen || true)}"

# `--bpf-program` writes only the program account. The upgradeable loader also
# asks for a ProgramData account, which the genesis never makes, so the program
# comes up as "not deployed". BPFLoader2 needs no second account, and nobody
# can swap the program and empty the vault.
LOADER="${LOADER:-BPFLoader2111111111111111111111111111111111}"

VAULT_SOL="${VAULT_SOL:-21000000}"

# The token programs and the DEX program come from Solana mainnet. Set DEX=0 to
# build a chain with the bridge alone.
DEX="${DEX:-1}"
DEX_PROGRAMS="${PROGRAMS:-$REPO/genesis/programs}"
TOKEN_ID=TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
ATA_ID=ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL
MEMO_ID=MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr
CPMM_ID=CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C

# SIMD-0357 burns a Validator Admission Ticket from every staked vote account
# at each epoch. That destroys money outside the peg, so the genesis switches
# it off. It only runs when `alpenglow` is also active, and that stays off too.
VAT_FEATURE="${VAT_FEATURE:-VAT9huvhPjRN9cyrPytq9rwvEJ3J4ADtjdncgZRyANJ}"
# The oracle seals the bundles, marks them paid, and pays the fees of the
# credit loop until a deposit funds it.
ORACLE_LAMPORTS="${ORACLE_LAMPORTS:-10000000}"

# The bridge config. BMM settles from ECASH_START_HEIGHT, and a new chain
# counts the deposits up to it as credited, so it is the eCash tip at the
# start. D is the eCash confirmations before a credit. A checkpoint at eCash
# height H counts only when its bank credited every deposit through H minus
# DEPOSIT_LAG. K and L are validator flags in run-validator.sh.
ECASH_START_HEIGHT="${ECASH_START_HEIGHT:-}"
DEPOSIT_CONFIRMATIONS="${DEPOSIT_CONFIRMATIONS:-6}"
DEPOSIT_LAG="${DEPOSIT_LAG:-100}"
# A validator leads only up to one epoch after its root, and a BMM root comes
# 30 to 120 minutes after its block. The development default of 8192 slots is
# about 55 minutes, so the chain stopped. 432000 slots is about 2 days.
SLOTS_PER_EPOCH="${SLOTS_PER_EPOCH:-432000}"
# The state file of a halted chain from `migrate-state`. The new genesis then
# carries its balances, its open withdrawals, and its bridge cursor.
STATE="${STATE:-}"
# A bundle waits this many slots before the oracle can mark it paid, so a
# stolen oracle key closes at most one bundle in that time. eCash pays a
# bundle after 6049 blocks, about 9 million slots.
BUNDLE_MIN_SLOTS="${BUNDLE_MIN_SLOTS:-4000000}"

# These lamports carry no Bitcoin behind them. The bridge counts them as
# float, and no withdrawal lands until gifts to the vault back all of it.
# The identity pays the fees of the settles until BMM pays it.
BOOTSTRAP_LAMPORTS="${BOOTSTRAP_LAMPORTS:-10000000}"
STAKE_LAMPORTS="${STAKE_LAMPORTS:-100000}"
# Agave 4 takes LAMPORTS_PER_BYTE_YEAR as the price of one byte, with an
# exemption threshold of one. (128 + data_len) * LAMPORTS_PER_BYTE_YEAR gives
# the rent-exempt minimum. The stock price of 6960 makes an empty account cost
# 890880 lamports, which is 89088 satoshis. Four makes it 512 lamports.
FAUCET_LAMPORTS="${FAUCET_LAMPORTS:-1024}"

LAMPORTS_PER_BYTE_YEAR="${LAMPORTS_PER_BYTE_YEAR:-4}"
# `solana-genesis` gives the vote account the rent of its 3762 bytes.
VOTE_LAMPORTS=$(((128 + 3762) * LAMPORTS_PER_BYTE_YEAR))

# One satoshi per signature. The stock 5000 lamports is 500 satoshis.
LAMPORTS_PER_SIGNATURE="${LAMPORTS_PER_SIGNATURE:-10}"

if [ -z "$SOLANA_GENESIS" ] || [ ! -x "$SOLANA_GENESIS" ]; then
  echo "error: solana-genesis is missing. No Agave CLI release carries it." >&2
  echo "Build it from an agave checkout, then set SOLANA_GENESIS:" >&2
  echo "  cargo build --release --bin solana-genesis --bin agave-validator" >&2
  exit 1
fi
if [ -z "$SOLANA_KEYGEN" ]; then
  echo "error: solana-keygen is not on the PATH." >&2
  exit 1
fi

if [ ! -f "$PROGRAM_SO" ]; then
  echo "error: the bridge program is not at $PROGRAM_SO" >&2
  echo "Build it first: cargo-build-sbf --manifest-path programs/bridge/Cargo.toml --sbf-out-dir target/deploy" >&2
  exit 1
fi

if [ -z "$ECASH_START_HEIGHT" ]; then
  echo "error: set ECASH_START_HEIGHT to the eCash tip, for example from" >&2
  echo "  sol-drivechain-daemon ecash-tip --network <network> --enforcer-url <url> --slot <slot>" >&2
  exit 1
fi

if [ ! -x "$DAEMON" ]; then
  echo "error: the daemon is not at $DAEMON" >&2
  echo "Build it first: cargo build --manifest-path daemon/Cargo.toml" >&2
  exit 1
fi

mkdir -p "$KEYS"
for name in validator-identity validator-vote validator-stake oracle faucet; do
  if [ ! -f "$KEYS/$name.json" ]; then
    "$SOLANA_KEYGEN" new --no-bip39-passphrase --silent -o "$KEYS/$name.json"
    echo "made a new key: $name"
  fi
done

PROGRAM_ID="$(cat "$KEYS/bridge-program.pubkey")"
IDENTITY="$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-identity.json")"
FAUCET="$("$SOLANA_KEYGEN" pubkey "$KEYS/faucet.json")"

DEX_FLAGS=()
DEX_ARGS=()
if [ "$DEX" = "1" ]; then
  # A token account takes 165 bytes, and this rent schedule makes it exempt.
  PROGRAMS="$DEX_PROGRAMS" \
    FEE_OWNER="$("$SOLANA_KEYGEN" pubkey "$KEYS/faucet.json")" \
    FEE_LAMPORTS=$(((128 + 165) * LAMPORTS_PER_BYTE_YEAR * 2)) \
    bash "$REPO/genesis/fetch-dex-programs.sh"
  DEX_ARGS=(--dex-accounts "$REPO/genesis/dex-accounts.yaml")
  DEX_FLAGS=(
    --bpf-program "$TOKEN_ID" "$LOADER" "$DEX_PROGRAMS/token.so"
    --bpf-program "$ATA_ID" "$LOADER" "$DEX_PROGRAMS/ata.so"
    --bpf-program "$MEMO_ID" "$LOADER" "$DEX_PROGRAMS/memo.so"
    --bpf-program "$CPMM_ID" "$LOADER" "$DEX_PROGRAMS/cpmm.so"
  )
fi

# The genesis writes the bridge config and the block record, so no
# transaction creates them and the runtime never mints their rent.
STATE_ARGS=()
if [ -n "$STATE" ]; then
  STATE_ARGS=(--state "$STATE")
  # `solana-genesis` writes the bootstrap accounts after the primordial
  # accounts. The old identity holds the lamports of the old stake and vote
  # accounts, so the new ones come out of it and no lamport is new.
  OLD_IDENTITY_LAMPORTS="$("$DAEMON" state-lamports --state "$STATE" --pubkey "$IDENTITY")"
  if [ "$OLD_IDENTITY_LAMPORTS" -lt $((STAKE_LAMPORTS + VOTE_LAMPORTS)) ]; then
    echo "error: the old identity holds $OLD_IDENTITY_LAMPORTS lamports, below the new stake and vote accounts" >&2
    exit 1
  fi
  BOOTSTRAP_LAMPORTS=$((OLD_IDENTITY_LAMPORTS - STAKE_LAMPORTS - VOTE_LAMPORTS))
  FLOAT_LAMPORTS=0
  # `solana-genesis` always makes a faucet of at least one lamport, and the
  # primordial accounts load after it. So the faucet goes on the vault, which
  # the state replaces, and the old faucet keeps its balance as a plain account.
  FAUCET_FLAGS=(--faucet-pubkey "$("$DAEMON" derive --program-id "$PROGRAM_ID" | awk '/^vault/ {print $2}')" \
    --faucet-lamports 1)
else
  FLOAT_LAMPORTS=$((BOOTSTRAP_LAMPORTS + STAKE_LAMPORTS + VOTE_LAMPORTS + FAUCET_LAMPORTS))
  FAUCET_FLAGS=(--faucet-pubkey "$FAUCET" --faucet-lamports "$FAUCET_LAMPORTS")
fi
"$DAEMON" genesis --program-id "$PROGRAM_ID" --vault-sol "$VAULT_SOL" \
  --oracle "$("$SOLANA_KEYGEN" pubkey "$KEYS/oracle.json")" --oracle-lamports "$ORACLE_LAMPORTS" \
  --float-lamports "$FLOAT_LAMPORTS" --bundle-min-slots "$BUNDLE_MIN_SLOTS" \
  --lamports-per-byte-year "$LAMPORTS_PER_BYTE_YEAR" \
  --ecash-start-height "$ECASH_START_HEIGHT" \
  --deposit-confirmations "$DEPOSIT_CONFIRMATIONS" --deposit-lag "$DEPOSIT_LAG" \
  ${STATE_ARGS[@]+"${STATE_ARGS[@]}"} ${DEX_ARGS[@]+"${DEX_ARGS[@]}"} \
  --out "$REPO/genesis/primordial.yaml"

rm -rf "$LEDGER"
mkdir -p "$LEDGER"

"$SOLANA_GENESIS" \
  --ledger "$LEDGER" \
  --cluster-type development \
  --slots-per-epoch "$SLOTS_PER_EPOCH" \
  --bootstrap-validator \
    "$IDENTITY" \
    "$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-vote.json")" \
    "$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-stake.json")" \
  --bootstrap-validator-lamports "$BOOTSTRAP_LAMPORTS" \
  --bootstrap-validator-stake-lamports "$STAKE_LAMPORTS" \
  ${FAUCET_FLAGS[@]+"${FAUCET_FLAGS[@]}"} \
  --inflation none \
  --fee-burn-percentage 0 \
  --rent-burn-percentage 0 \
  --lamports-per-byte-year "$LAMPORTS_PER_BYTE_YEAR" \
  --target-lamports-per-signature "$LAMPORTS_PER_SIGNATURE" \
  --deactivate-feature "$VAT_FEATURE" \
  --primordial-accounts-file "$REPO/genesis/primordial.yaml" \
  --bpf-program "$PROGRAM_ID" "$LOADER" "$PROGRAM_SO" \
  ${DEX_FLAGS[@]+"${DEX_FLAGS[@]}"}

echo
echo "the ledger is at $LEDGER"
echo "the oracle is $("$SOLANA_KEYGEN" pubkey "$KEYS/oracle.json")"
echo "the identity starts with $BOOTSTRAP_LAMPORTS lamports"
"$DAEMON" derive --program-id "$PROGRAM_ID"
