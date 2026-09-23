#!/usr/bin/env bash
# Proves the DEX on a running regtest chain: the programs are there, the faucet
# pays, and a token, a pool, and two trades work.
#
# Bring the chain up first:
#   bash scripts/regtest-chain.sh up
#   bash scripts/regtest-dex.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

SOLANA_RPC_PORT="${SOLANA_RPC_PORT:-8799}"
SOLANA_URL="${SOLANA_URL:-http://127.0.0.1:$SOLANA_RPC_PORT}"
SOLANA="${SOLANA:-$(command -v solana || true)}"

TOKEN_ID=TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
ATA_ID=ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL
MEMO_ID=MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr
CPMM_ID=CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C
CPMM_CONFIG_ID=D4FPEruKEHrG5TenZ2mpDGEfu1iUvTiqBxvpU8HLBvC2
WSOL_ID=So11111111111111111111111111111111111111112

fail() { echo "FAIL: $1" >&2; exit 1; }
step() { echo; echo "== $1"; }

command -v node >/dev/null || fail "node is missing, and the proof runs on node"
[ -n "$SOLANA" ] || fail "solana is not on the PATH"

step "the chain answers"
"$SOLANA" -u "$SOLANA_URL" slot >/dev/null || fail "the chain gave no slot"
echo "slot $("$SOLANA" -u "$SOLANA_URL" slot)"

step "the chain carries the programs"
for pair in "SPL Token:$TOKEN_ID" "associated token account:$ATA_ID" "memo:$MEMO_ID" "CP-Swap:$CPMM_ID"; do
  name="${pair%%:*}"
  id="${pair#*:}"
  line="$("$SOLANA" -u "$SOLANA_URL" account "$id" 2>/dev/null | awk '/^Executable/ {print $2}')"
  [ "$line" = "true" ] || fail "the $name program is not executable at $id"
  echo "PASS: the $name program runs at $id"
done

step "the chain carries the DEX accounts"
OWNER="$("$SOLANA" -u "$SOLANA_URL" account "$CPMM_CONFIG_ID" 2>/dev/null | awk '/^Owner/ {print $2}')"
[ "$OWNER" = "$CPMM_ID" ] || fail "the CP-Swap fee config belongs to $OWNER, not to $CPMM_ID"
echo "PASS: the CP-Swap fee config is at $CPMM_CONFIG_ID"
OWNER="$("$SOLANA" -u "$SOLANA_URL" account "$WSOL_ID" 2>/dev/null | awk '/^Owner/ {print $2}')"
[ "$OWNER" = "$TOKEN_ID" ] || fail "the wrapped BTC mint belongs to $OWNER, not to the token program"
echo "PASS: the wrapped BTC mint is at $WSOL_ID"

step "the client builds"
if [ ! -f "$REPO/dex/dist/scripts/trade.mjs" ]; then
  (cd "$REPO/dex" && npm install --no-audit --no-fund >/dev/null && npm run build >/dev/null)
fi
[ -f "$REPO/dex/dist/scripts/trade.mjs" ] || fail "the build wrote no trade script"
echo "PASS: dex/dist holds the page and the scripts"

step "a token, a pool, and two trades"
RPC_URL="$SOLANA_URL" node "$REPO/dex/dist/scripts/trade.mjs" || fail "the trade proof failed"

echo
echo "PASS: the DEX works on this chain"
