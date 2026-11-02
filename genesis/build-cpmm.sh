#!/usr/bin/env bash
# Builds Raydium CP-Swap from a pinned commit, with each authority key set to
# NO_KEY. NO_KEY is off the ed25519 curve, so no signature can match it.
#
# The build runs in the pinned Solana verifiable build image, so each host
# gets the same bytes. dex-programs.sha256 holds the hash of those bytes.
#
#   bash genesis/build-cpmm.sh [out]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$HERE/programs/cpmm.so}"
MANIFEST="$HERE/dex-programs.sha256"

CPMM_REPO=https://github.com/raydium-io/raydium-cp-swap
CPMM_COMMIT=3d69122790e97cd6aff24e8ba95940793773f58c
IMAGE=solanafoundation/solana-verifiable-build:3.1.10@sha256:f71be5ca7620b7e40933b7f1294fa44e01d08c1fc5ba1f375a2478f5a01580d3
NO_KEY="$(python3 -I "$HERE/dex_accounts.py" no-key)"

# The mainnet keys that sign for an admin action: the admin, the lamport
# collector, the fee owners, and the owners of the permission, creator fee
# share, and mint support accounts.
AUTHORITIES=(
  GThUX1Atko4tqhN2NaiTazWSeFWMuiUvfFnyJyUghFMJ
  RayGkhY93thaTgCv98sx1pNLgBHhJDxWUeZXp4bjmnp
  FUNDduJTA7XcckKHKfAoEnnhuSud2JUCUZv6opWEjrBU
  ProCXqRcXJjoUd1RNoo28bSizAA6EEqt9wURZYPDc5u
  RayRrPVNAg3hTPa1yfZiR49FqCpkmiLaP4yWsVBnoBZ
  RayqjDRsNEFuPcDE4JpEScwJvwusmHYuNZ3MgET4D7U
  RayFfEhJHWToYRsr8sjVZZkdddBeYxyJazDitpdE7zJ
  Rayv2LG4tFSMizZhMP8aSUYxDPjV8qJtx2NQY9RKYZy
)
# Each key literal that may stay in the source after the change: the devnet
# keys, which the build leaves out, the pool fee token account, and NO_KEY.
ALLOWED_KEYS="3oE58BKVt8KuYkGxx8zBojugnymWmBiyafWgMrnb6eYy
$NO_KEY
DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8
DRay2aRSqmGVMkcvsQNU4iskM1ztX31EP7pyrtrntqBQ
DRay33UmULQCeawH3dVpJfN3uqLj6Qtq4ymSRx2pAgGK
DRayDe7AGe6nFg6egr7D42ECyDf15GV52WFwZAbfUo1R
DRayJkSKsijbcEqdooK4uUGcT6gjbEuwUh7V6Nmqct7M
DRaydJNq54dSDHUqYCE3G8YySgaXfZucbh7dTXw9fBMs
DRaypyeDL6y1dUusMgwyeDM5JebjhsSi8aRXobKQ9DcQ
DRayqG9RXYi8WHgWEmRQGrUWRWbhjYWYkCRJDd6JBBak"

fail() { echo "error: $1" >&2; exit 1; }
command -v docker >/dev/null || fail "docker is not on the PATH"
command -v git >/dev/null || fail "git is not on the PATH"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

git -C "$WORK" init -q
git -C "$WORK" fetch -q --depth 1 "$CPMM_REPO" "$CPMM_COMMIT"
git -C "$WORK" checkout -q FETCH_HEAD
[ "$(git -C "$WORK" rev-parse HEAD)" = "$CPMM_COMMIT" ] || fail "the checkout is not $CPMM_COMMIT"

SRC="$WORK/programs/cp-swap/src"
for key in "${AUTHORITIES[@]}"; do
  grep -rlF "$key" "$SRC" >/dev/null || fail "the source does not hold $key"
  grep -rlF "$key" "$SRC" | while read -r file; do
    KEY="$key" NO_KEY="$NO_KEY" perl -pi -e 's/\Q$ENV{KEY}\E/$ENV{NO_KEY}/g' "$file"
  done
done
FOUND_KEYS="$(grep -rhoE 'pubkey!\("[1-9A-HJ-NP-Za-km-z]+"\)' "$SRC" \
  | sed -E 's/pubkey!\("(.*)"\)/\1/' | LC_ALL=C sort -u)"
[ "$FOUND_KEYS" = "$(printf '%s\n' "$ALLOWED_KEYS" | LC_ALL=C sort -u)" ] \
  || fail "the source holds a key outside the allowed list: $FOUND_KEYS"

# The image builds as root, so the build gives the files back to this user.
docker run --rm --platform linux/amd64 -v "$WORK":/build -w /build \
  -e OWNER="$(id -u):$(id -g)" "$IMAGE" sh -c \
  'cargo build-sbf --manifest-path programs/cp-swap/Cargo.toml -- --locked; status=$?; chown -R "$OWNER" /build/target; exit $status'

BUILT="$WORK/target/deploy/raydium_cp_swap.so"
[ -s "$BUILT" ] || fail "the build wrote no program"
WANT="$(awk '$2 == "cpmm.so" {print $1}' "$MANIFEST")"
GOT="$(sha256sum "$BUILT" | awk '{print $1}')"
[ -n "$WANT" ] || fail "$MANIFEST holds no hash for cpmm.so"
[ "$GOT" = "$WANT" ] || fail "the build gave $GOT, and $MANIFEST pins $WANT"
mkdir -p "$(dirname "$OUT")"
cp "$BUILT" "$OUT"
echo "built CP-Swap $CPMM_COMMIT with no admin key at $OUT"
