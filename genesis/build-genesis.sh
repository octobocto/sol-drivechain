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
# The oracle marks the paid withdrawals and pays the fees of the credit loop.
ORACLE_SOL="${ORACLE_SOL:-1}"

# The bridge config. BMM settles from ECASH_START_HEIGHT, and a new chain
# counts the deposits up to it as credited, so it is the eCash tip at the
# start. D is the eCash confirmations before a credit. A checkpoint at eCash
# height H counts only when its bank credited every deposit through H minus
# DEPOSIT_LAG. K and L are validator flags in run-validator.sh.
ECASH_START_HEIGHT="${ECASH_START_HEIGHT:-}"
DEPOSIT_CONFIRMATIONS="${DEPOSIT_CONFIRMATIONS:-6}"
DEPOSIT_LAG="${DEPOSIT_LAG:-100}"
# The state file of a halted chain from `migrate-state`. The new genesis then
# carries its balances, its open withdrawals, and its deposit cursor.
STATE="${STATE:-}"

# These lamports carry no Bitcoin behind them. The bridge counts pegged
# lamports and subtracts with a checked subtraction, so they can never peg out.
# Every fee goes to the bridge treasury, so the validator only holds a buffer
# for its votes, not a running cost.
BOOTSTRAP_LAMPORTS="${BOOTSTRAP_LAMPORTS:-1000000000}"
STAKE_LAMPORTS="${STAKE_LAMPORTS:-100000}"
# Agave 4 takes LAMPORTS_PER_BYTE_YEAR as the price of one byte, with an
# exemption threshold of one. (128 + data_len) * LAMPORTS_PER_BYTE_YEAR gives
# the rent-exempt minimum. The stock price of 6960 makes an empty account cost
# 890880 lamports, which is 89088 satoshis. Four makes it 512 lamports.
FAUCET_LAMPORTS="${FAUCET_LAMPORTS:-1024}"

LAMPORTS_PER_BYTE_YEAR="${LAMPORTS_PER_BYTE_YEAR:-4}"

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

# The genesis writes the bridge config and the block record, so no
# transaction creates them and the runtime never mints their rent.
STATE_ARGS=()
if [ -n "$STATE" ]; then
  STATE_ARGS=(--state "$STATE")
  # `solana-genesis` writes the bootstrap identity after the primordial
  # accounts, so the identity keeps its old balance only through this flag.
  OLD_IDENTITY_LAMPORTS="$("$DAEMON" state-lamports --state "$STATE" --pubkey "$IDENTITY")"
  if [ "$OLD_IDENTITY_LAMPORTS" != 0 ]; then
    BOOTSTRAP_LAMPORTS="$OLD_IDENTITY_LAMPORTS"
  fi
fi
"$DAEMON" genesis --program-id "$PROGRAM_ID" --vault-sol "$VAULT_SOL" \
  --oracle "$("$SOLANA_KEYGEN" pubkey "$KEYS/oracle.json")" --oracle-sol "$ORACLE_SOL" \
  --lamports-per-byte-year "$LAMPORTS_PER_BYTE_YEAR" \
  --ecash-start-height "$ECASH_START_HEIGHT" \
  --deposit-confirmations "$DEPOSIT_CONFIRMATIONS" --deposit-lag "$DEPOSIT_LAG" \
  ${STATE_ARGS[@]+"${STATE_ARGS[@]}"} \
  --out "$REPO/genesis/primordial.yaml"

DEX_FLAGS=()
if [ "$DEX" = "1" ]; then
  # A token account takes 165 bytes, and this rent schedule makes it exempt.
  PROGRAMS="$DEX_PROGRAMS" \
    FEE_OWNER="$("$SOLANA_KEYGEN" pubkey "$KEYS/faucet.json")" \
    FEE_LAMPORTS=$(((128 + 165) * LAMPORTS_PER_BYTE_YEAR * 2)) \
    bash "$REPO/genesis/fetch-dex-programs.sh"
  cat "$REPO/genesis/dex-accounts.yaml" >> "$REPO/genesis/primordial.yaml"
  DEX_FLAGS=(
    --bpf-program "$TOKEN_ID" "$LOADER" "$DEX_PROGRAMS/token.so"
    --bpf-program "$ATA_ID" "$LOADER" "$DEX_PROGRAMS/ata.so"
    --bpf-program "$MEMO_ID" "$LOADER" "$DEX_PROGRAMS/memo.so"
    --bpf-program "$CPMM_ID" "$LOADER" "$DEX_PROGRAMS/cpmm.so"
  )
fi

rm -rf "$LEDGER"
mkdir -p "$LEDGER"

"$SOLANA_GENESIS" \
  --ledger "$LEDGER" \
  --cluster-type development \
  --bootstrap-validator \
    "$IDENTITY" \
    "$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-vote.json")" \
    "$("$SOLANA_KEYGEN" pubkey "$KEYS/validator-stake.json")" \
  --bootstrap-validator-lamports "$BOOTSTRAP_LAMPORTS" \
  --bootstrap-validator-stake-lamports "$STAKE_LAMPORTS" \
  --faucet-pubkey "$("$SOLANA_KEYGEN" pubkey "$KEYS/faucet.json")" \
  --faucet-lamports "$FAUCET_LAMPORTS" \
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
