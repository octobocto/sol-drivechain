#!/usr/bin/env bash
# Puts the DEX programs in PROGRAMS, and writes the genesis accounts that the
# DEX needs.
#
# The token programs come from Solana mainnet at their mainnet addresses, so
# every wallet and every SDK finds them where it expects. CP-Swap comes from
# build-cpmm.sh, with no admin key. dex-programs.sha256 pins the bytes of each
# program, and a program that does not match it stops the script.
#
#   LAMPORTS_PER_BYTE_YEAR=4 bash genesis/fetch-dex-programs.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PROGRAMS="${PROGRAMS:-$HERE/programs}"
MAINNET_URL="${MAINNET_URL:-https://api.mainnet-beta.solana.com}"
MANIFEST="$HERE/dex-programs.sha256"
ACCOUNTS="$HERE/dex-accounts.yaml"
LAMPORTS_PER_BYTE_YEAR="${LAMPORTS_PER_BYTE_YEAR:-}"

fail() { echo "error: $1" >&2; exit 1; }
[[ "$LAMPORTS_PER_BYTE_YEAR" =~ ^[1-9][0-9]*$ ]] || fail "set LAMPORTS_PER_BYTE_YEAR to the rent price of the genesis"

pinned() {
  awk -v name="$1" '$2 == name {print $1}' "$MANIFEST"
}

# Fails unless the file holds the bytes that the manifest pins.
check_pin() {
  local name="$1" path="$2" want got
  want="$(pinned "$name")"
  [[ "$want" =~ ^[0-9a-f]{64}$ ]] || fail "$MANIFEST holds no hash for $name"
  got="$(sha256sum "$path" | awk '{print $1}')"
  [ "$got" = "$want" ] || fail "$path has sha256 $got, and $MANIFEST pins $want"
}

mkdir -p "$PROGRAMS"

dump_program() {
  local name="$1" id="$2" path="$PROGRAMS/$1.so" fresh
  if [ ! -s "$path" ]; then
    command -v solana >/dev/null || fail "solana is not on the PATH"
    echo "copying $name from $id"
    fresh="$(mktemp "$PROGRAMS/.$name.XXXXXX")"
    solana program dump -u "$MAINNET_URL" "$id" "$fresh" >/dev/null \
      || { rm -f "$fresh"; fail "the copy of $name failed"; }
    (check_pin "$name.so" "$fresh") || { rm -f "$fresh"; exit 1; }
    mv "$fresh" "$path"
  fi
  check_pin "$name.so" "$path"
}

dump_program token TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
dump_program token-2022 TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb
dump_program ata ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL
dump_program memo MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr

if [ ! -s "$PROGRAMS/cpmm.so" ]; then
  bash "$HERE/build-cpmm.sh" "$PROGRAMS/cpmm.so"
fi
check_pin cpmm.so "$PROGRAMS/cpmm.so"
echo "the programs match $MANIFEST"

# The CP-Swap config, the wrapped SOL mint, and the pool fee account. The
# script builds each one from constants, so no outside data enters the genesis.
python3 -I "$HERE/dex_accounts.py" accounts "$LAMPORTS_PER_BYTE_YEAR" "$ACCOUNTS"

echo "the programs are in $PROGRAMS"
echo "the genesis accounts are in $ACCOUNTS"
