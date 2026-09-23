#!/usr/bin/env bash
# Builds the three Agave binaries that no CLI release carries, with the one
# patch that this chain needs.
#
# Only the peg may change the money supply:
#
# 1. Upstream hard-codes a 50 percent burn of every transaction fee. A burnt
#    fee destroys pegged Bitcoin. The burn goes to zero.
# 2. The genesis funds each vote account with a 160 SOL Validator Admission
#    Ticket reserve. The genesis deactivates SIMD-0357, so no ticket is ever
#    burnt, and the reserve would be money that no Bitcoin backs.
#
# BMM pays the fees to eCash miners:
#
# 3. Every fee goes to the bridge treasury, not to the leader.
# 4. A simple vote pays no signature fee.
# 5. The crate `sol-drivechain-bmm` reads settled eCash heights from the
#    local enforcer. The bank checks each `settle_bmm` against it, and the
#    syscall `sol_get_bmm_commitment` answers the bridge from it.
# 6. The validator takes `--bmm-enforcer-url` and `--bmm-sidechain-slot`.
#
# To move to a later Agave, raise AGAVE_TAG and run this again.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGAVE_TAG="${AGAVE_TAG:-v4.2.2}"
AGAVE_DIR="${AGAVE_DIR:-$HOME/src/agave}"
AGAVE_REPO="${AGAVE_REPO:-https://github.com/anza-xyz/agave.git}"
PATCH="$HERE/agave-sol-drivechain.patch"

# Puts the checkout on the tag, with no local change left over.
#
# A CI cache restores `target/` on its own, so the directory can hold a build
# and no repository. The test names `.git`, and not the directory.
prepare_checkout() {
  if [ ! -d "$AGAVE_DIR/.git" ]; then
    mkdir -p "$AGAVE_DIR"
    git -C "$AGAVE_DIR" init -q
    git -C "$AGAVE_DIR" remote add origin "$AGAVE_REPO"
  fi
  # One shallow tag keeps the checkout small and pins the commit. A failure
  # here must stop the build, because a wrong commit gives a wrong validator.
  git -C "$AGAVE_DIR" fetch --depth 1 --force origin \
    "refs/tags/$AGAVE_TAG:refs/tags/$AGAVE_TAG"
  # `--force` throws away the patch of an earlier run.
  git -C "$AGAVE_DIR" checkout -q --force "$AGAVE_TAG"
}

prepare_checkout
cd "$AGAVE_DIR"
echo "agave $AGAVE_TAG is commit $(git rev-parse HEAD)"

# `git apply` writes nothing when one hunk fails, so the tree stays on the tag.
if ! git apply "$PATCH"; then
  echo "error: the patch does not apply to $AGAVE_TAG." >&2
  echo "Apply it to $AGAVE_TAG by hand, then write it again with git diff." >&2
  exit 1
fi
echo "the patch applies to $AGAVE_TAG"

cargo build --release --bin solana-genesis --bin agave-validator --bin solana-faucet

echo
echo "solana-genesis   $AGAVE_DIR/target/release/solana-genesis"
echo "agave-validator  $AGAVE_DIR/target/release/agave-validator"
echo "solana-faucet    $AGAVE_DIR/target/release/solana-faucet"
