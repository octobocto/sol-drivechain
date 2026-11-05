# shellcheck shell=bash
# The depths of each network, from the README table. A value in the
# environment wins. A network without values in the table asks for each one.
#
#   D    DEPOSIT_CONFIRMATIONS  eCash confirmations before a credit
#   lag  DEPOSIT_LAG            a checkpoint at H needs the credits through H - lag
#   N    BMM_CONFIRMATIONS      eCash blocks on top of a block before its settle
#   K    BMM_ROOT_DEPTH         eCash confirmations before a checkpoint is the root
#   L    BMM_FALLBACK_BLOCKS    eCash blocks without a checkpoint before the fallback

network_depths() {
  local d lag n k l
  case "${NETWORK:-}" in
    ecash) d=12 lag=36 n=6 k=3 l=12 ;;
    betanet | regtest) d=6 lag=18 n=6 k=2 l=6 ;;
    alphanet | mainnet | testnet | signet) d="" lag="" n="" k="" l="" ;;
    *)
      echo "error: set NETWORK to ecash, betanet, alphanet, mainnet, testnet, signet, or regtest" >&2
      return 1
      ;;
  esac
  DEPOSIT_CONFIRMATIONS="${DEPOSIT_CONFIRMATIONS:-$d}"
  DEPOSIT_LAG="${DEPOSIT_LAG:-$lag}"
  BMM_CONFIRMATIONS="${BMM_CONFIRMATIONS:-$n}"
  BMM_ROOT_DEPTH="${BMM_ROOT_DEPTH:-$k}"
  BMM_FALLBACK_BLOCKS="${BMM_FALLBACK_BLOCKS:-$l}"
}

# Fails unless each named variable holds a positive whole number.
require_depths() {
  local name value
  for name in "$@"; do
    value="${!name:-}"
    if ! [[ "$value" =~ ^[1-9][0-9]*$ ]]; then
      echo "error: set $name to a positive whole number. It is '$value' on $NETWORK." >&2
      return 1
    fi
  done
}
