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
# 4. A simple vote pays no signature fee only when its vote account holds stake
#    and its authorized voter signs it.
# 5. The crate `sol-drivechain-bmm` reads the active eCash chain from the
#    local enforcer. Before a `settle_bmm` runs, the bank asks it for the
#    commitment of that eCash block, and it puts the answer into the
#    `BmmAnswer` account of the tx.
# 6. The validator takes `--bmm-enforcer-url`. The genesis holds the sidechain
#    slot, N, K, and L in the `BmmParams` account, from the `solana-genesis`
#    flags `--bmm-sidechain-slot`, `--bmm-confirmations`, `--bmm-root-depth`,
#    and `--bmm-fallback-blocks`.
# 7. BMM checkpoints select the fork and move the root. After L eCash blocks
#    without a commitment, the stake votes move the root. A checkpoint counts
#    for the fork choice only after this node replayed its bank. A branch that
#    splits behind the root and holds more checkpoints, also for banks that
#    this node does not hold, stops the validator with exit code 75, and the
#    next start loads an older snapshot. After a restart the stake votes move
#    no root until its bank arrives or the next eCash block comes, and a
#    hold that brings no bank blocks a new hold for one block. Each restart
#    needs a new checkpoint K blocks deep.
#    The RPC `finalized` commitment is the newest root that BMM moved.
# 8. A commitment h* counts only when a pair (bank hash, payee) with
#    SHA-256(bank hash ‖ payee) = h* is known. The RPC method `bmmPublishPairs`
#    takes the pairs. The validator sends them to the RPC of every staked
#    peer, and it reads the pairs of each staked peer with `bmmGetPairs`. The
#    runtime writes a block record with the deposit credits of each recorded bank,
#    and a settle pays only a block in the record of its own fork. The genesis
#    holds the record account, so the runtime never mints its rent.
# 9. Before a `credit_deposits` runs, the bank asks the local enforcer for the
#    deposits of that eCash block, after D confirmations from the bridge
#    config. A checkpoint counts only when its bank credited every deposit
#    through H - lag, and the active eCash chain still holds those credits.
#    No leader builds on a bank whose credits the active chain dropped. A
#    settle gets the same rule 3 answer for the credits of its record entry.
# 10. Each validator keeps the answer of each settle and credit in the file
#    `bmm-answers` of its ledger. A replay of a slot at or below the root of
#    the start takes that answer. An eCash reorg after the first run then
#    cannot stop a restart or a replay from the genesis.
# 11. No feature account can turn on SIMD-0357, the inflation features, or
#    Alpenglow, and the incinerator burns nothing.
#
# To move to a later Agave, raise AGAVE_TAG and AGAVE_COMMIT and run this
# again.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGAVE_TAG="${AGAVE_TAG:-v4.2.2}"
# A tag can move, so the build checks the commit too.
AGAVE_COMMIT="${AGAVE_COMMIT:-c9c6f3287e26f24e3476e13e751aebe710191e89}"
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
if [ "$(git rev-parse HEAD)" != "$AGAVE_COMMIT" ]; then
  echo "error: $AGAVE_TAG is commit $(git rev-parse HEAD), not $AGAVE_COMMIT." >&2
  exit 1
fi
echo "agave $AGAVE_TAG is commit $AGAVE_COMMIT"

# `git apply` writes nothing when one hunk fails, so the tree stays on the tag.
if ! git apply "$PATCH"; then
  echo "error: the patch does not apply to $AGAVE_TAG." >&2
  echo "Apply it to $AGAVE_TAG by hand, then write it again with git diff." >&2
  exit 1
fi
echo "the patch applies to $AGAVE_TAG"

cargo build --release --locked --bin solana-genesis --bin agave-validator --bin solana-faucet

echo
echo "solana-genesis   $AGAVE_DIR/target/release/solana-genesis"
echo "agave-validator  $AGAVE_DIR/target/release/agave-validator"
echo "solana-faucet    $AGAVE_DIR/target/release/solana-faucet"
