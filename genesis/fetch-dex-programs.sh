#!/usr/bin/env bash
# Copies the token programs and the DEX program from Solana mainnet, and writes
# the genesis accounts that the DEX needs.
#
# The chain loads each program at its mainnet address, so every wallet and every
# SDK finds it where it expects. CP-Swap keeps its fee parameters in an
# `AmmConfig` account that only its admin can create, so the genesis carries a
# copy of the mainnet account. The address is a PDA of the program, and the
# program address is the same here, so the copy lands at the correct key.
#
#   bash genesis/fetch-dex-programs.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

PROGRAMS="${PROGRAMS:-$HERE/programs}"
MAINNET_URL="${MAINNET_URL:-https://api.mainnet-beta.solana.com}"
MANIFEST="$HERE/dex-programs.sha256"
ACCOUNTS="$HERE/dex-accounts.yaml"

TOKEN_ID=TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
ATA_ID=ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL
MEMO_ID=MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr
CPMM_ID=CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C
# The CP-Swap fee config at index 0. It takes 25 basis points of each trade.
CPMM_CONFIG_ID=D4FPEruKEHrG5TenZ2mpDGEfu1iUvTiqBxvpU8HLBvC2
# CP-Swap sends the pool creation fee to this address.
CPMM_FEE_ID=DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8
# The wrapped SOL mint. CP-Swap trades tokens, so a trade of the native coin
# goes through a wrapped account. The mint must exist from the first block.
WSOL_ID=So11111111111111111111111111111111111111112

fail() { echo "error: $1" >&2; exit 1; }
command -v solana >/dev/null || fail "solana is not on the PATH"

mkdir -p "$PROGRAMS"

dump_program() {
  local name="$1" id="$2" path="$PROGRAMS/$1.so"
  if [ -s "$path" ]; then
    echo "$name is here already"
    return 0
  fi
  echo "copying $name from $id"
  solana program dump -u "$MAINNET_URL" "$id" "$path" >/dev/null \
    || fail "the copy of $name failed"
}

dump_program token "$TOKEN_ID"
dump_program ata "$ATA_ID"
dump_program memo "$MEMO_ID"
dump_program cpmm "$CPMM_ID"

# The manifest pins the bytes of each program. A silent change of a program on a
# chain that holds money is a danger, so the check runs every time.
if [ -f "$MANIFEST" ]; then
  (cd "$PROGRAMS" && sha256sum --check --status "$MANIFEST") \
    || fail "a program does not match $MANIFEST. Delete $PROGRAMS to copy again."
  echo "the programs match the manifest"
else
  (cd "$PROGRAMS" && sha256sum token.so ata.so memo.so cpmm.so) > "$MANIFEST"
  echo "wrote a new manifest at $MANIFEST"
fi

dump_account() {
  local name="$1" id="$2" path="$PROGRAMS/$1.json"
  if [ -s "$path" ]; then
    return 0
  fi
  echo "copying the $name account from $id"
  solana account -u "$MAINNET_URL" "$id" --output json --output-file "$path" >/dev/null \
    || fail "the copy of the $name account failed"
}

field() {
  python3 -c "import json,sys; a=json.load(open(sys.argv[1]))['account']; print(a['data'][0] if sys.argv[2]=='data' else a[sys.argv[2]])" \
    "$PROGRAMS/$1.json" "$2"
}

dump_account cpmm-config "$CPMM_CONFIG_ID"
dump_account wsol-mint "$WSOL_ID"
[ -n "$(field cpmm-config data)" ] || fail "the CP-Swap config account holds no data"
[ -n "$(field wsol-mint data)" ] || fail "the wrapped SOL mint holds no data"

cat > "$ACCOUNTS" <<YAML
$CPMM_CONFIG_ID:
  balance: $(field cpmm-config lamports)
  owner: $CPMM_ID
  data: "$(field cpmm-config data)"
  executable: false
$WSOL_ID:
  balance: $(field wsol-mint lamports)
  owner: $TOKEN_ID
  data: "$(field wsol-mint data)"
  executable: false
$CPMM_FEE_ID:
  balance: 1024
  owner: 11111111111111111111111111111111
  data: ""
  executable: false
YAML

echo "the programs are in $PROGRAMS"
echo "the genesis accounts are in $ACCOUNTS"
