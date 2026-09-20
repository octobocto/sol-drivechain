#!/usr/bin/env bash
# Prints the activation JSON of a published release.
#
# A BIP300 declaration names a release tarball and a build commit. This reads
# both from GitHub, so nobody types a hash by hand and nobody guesses which
# artifact the hash covers.
#
# A third party replicates both values:
#   hashid1  gh release view <tag> --json assets --jq '.assets[0].digest'
#   hashid2  gh api repos/<owner>/<repo>/commits/<tag> --jq .sha
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${1:-}"
SLOT="${SLOT:-8}"
GH_REPO="${GH_REPO:-octobocto/sol-drivechain}"
DAEMON="${DAEMON:-$REPO/daemon/target/release/sol-drivechain-daemon}"

if [ -z "$TAG" ]; then
  echo "use: $0 <tag>" >&2
  echo "example: $0 v0.1.0" >&2
  exit 1
fi

for tool in gh jq; do
  command -v "$tool" >/dev/null || { echo "error: $tool is missing" >&2; exit 1; }
done
[ -x "$DAEMON" ] || {
  echo "error: no daemon at $DAEMON" >&2
  echo "Build it: cargo build --release --manifest-path daemon/Cargo.toml" >&2
  exit 1
}

ASSETS="$(gh release view "$TAG" --repo "$GH_REPO" --json assets --jq '.assets')"
DIGEST="$(echo "$ASSETS" | jq -r '[.[] | select(.name | endswith(".tar.gz"))][0].digest // ""')"
if [ -z "$DIGEST" ]; then
  echo "error: release $TAG carries no .tar.gz asset with a digest" >&2
  exit 1
fi
HASHID1="${DIGEST#sha256:}"
if [ "$HASHID1" = "$DIGEST" ]; then
  echo "error: the asset digest is not sha256: $DIGEST" >&2
  exit 1
fi

HASHID2="$(gh api "repos/$GH_REPO/commits/$TAG" --jq '.sha')"
[ -n "$HASHID2" ] || { echo "error: no commit for tag $TAG" >&2; exit 1; }

exec "$DAEMON" activation-request \
  --slot "$SLOT" \
  --hashid1 "$HASHID1" \
  --hashid2 "$HASHID2"
