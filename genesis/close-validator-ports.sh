#!/usr/bin/env bash
# Drops outside traffic to the validator ports.
#
# The validator must bind 0.0.0.0, because it sends its own votes through its
# advertised address and a loopback bind stops them from landing. So the ports
# stay open on the socket and the packet filter closes them instead.
#
# These rules name one port range only. They change no default policy, so they
# cannot lock you out of SSH and they touch no other service on the host.
set -euo pipefail

FIRST_PORT="${FIRST_PORT:-8000}"
LAST_PORT="${LAST_PORT:-8020}"
CHAIN="${CHAIN:-INPUT}"
PORTS="$FIRST_PORT:$LAST_PORT"
# The comment holds no space. An iptables argument list must not word split.
TAG="sol-drivechain-validator"

command -v iptables >/dev/null || { echo "error: iptables is missing" >&2; exit 1; }

# The loopback rule comes first, so the host still reaches itself.
add_rules() {
  local protocol="$1"
  if ! iptables -C "$CHAIN" -i lo -p "$protocol" --dport "$PORTS" \
       -m comment --comment "$TAG-lo" -j ACCEPT 2>/dev/null; then
    iptables -I "$CHAIN" 1 -i lo -p "$protocol" --dport "$PORTS" \
      -m comment --comment "$TAG-lo" -j ACCEPT
  fi
  if ! iptables -C "$CHAIN" -p "$protocol" --dport "$PORTS" \
       -m comment --comment "$TAG" -j DROP 2>/dev/null; then
    iptables -A "$CHAIN" -p "$protocol" --dport "$PORTS" \
      -m comment --comment "$TAG" -j DROP
  fi
}

remove_rules() {
  local protocol="$1"
  while iptables -C "$CHAIN" -p "$protocol" --dport "$PORTS" \
        -m comment --comment "$TAG" -j DROP 2>/dev/null; do
    iptables -D "$CHAIN" -p "$protocol" --dport "$PORTS" \
      -m comment --comment "$TAG" -j DROP
  done
  while iptables -C "$CHAIN" -i lo -p "$protocol" --dport "$PORTS" \
        -m comment --comment "$TAG-lo" -j ACCEPT 2>/dev/null; do
    iptables -D "$CHAIN" -i lo -p "$protocol" --dport "$PORTS" \
      -m comment --comment "$TAG-lo" -j ACCEPT
  done
}

case "${1:-apply}" in
  apply)
    for protocol in tcp udp; do
      add_rules "$protocol"
    done
    echo "the validator ports $PORTS take loopback traffic only"
    ;;
  remove)
    for protocol in tcp udp; do
      remove_rules "$protocol"
    done
    echo "the rules are gone"
    ;;
  status)
    iptables -S "$CHAIN" | grep -F "$TAG" || echo "no rule holds the validator ports"
    ;;
  *)
    echo "use: $0 [apply|remove|status]" >&2
    exit 1
    ;;
esac
