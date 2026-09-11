#!/usr/bin/env bash
#
# spawn-verify.sh — `theater setup` (the no-init spawn-time path) each actor
# composite on the pinned theater host, asserting NONE of the build-green /
# spawn-fail class the `nix build` cannot catch: MissingInterfaceMetadata,
# InterfaceHashMismatch, Failed-to-instantiate-Pack, unknown-import.
#
# `theater setup` runs module_compile + interface-hash verify + handler wiring
# but does NOT call init (no listeners/ports bound). On SUCCESS a setup-only
# actor has nothing to terminate it, so it sits waiting — we cap with a timeout
# and treat "no failure signature in the output" as pass; a real failure exits
# fast with the signature. (theater-dev's design; iterate the grep on first run.)
#
# Run INSIDE `nix develop` (needs the theater CLI on PATH) after `nix build
# .#default` (result/inbox_<actor>.wasm). Rewrites each manifest's `package` ->
# the built wasm and store `base_path` -> a CI tmp dir (the dev/prod paths don't
# exist in CI). Non-blocking gate; the self-serve spawn gate for spine work.
#
# Usage: nix develop --command bash ops/spawn-verify.sh [result-dir]
set -uo pipefail

RESULT="${1:-result}"
command -v theater >/dev/null 2>&1 || { echo "theater CLI not on PATH (run inside 'nix develop')"; exit 2; }

store="$(mktemp -d)"; work="$(mktemp -d)"
trap 'rm -rf "$store" "$work"' EXIT

SIG='MissingInterfaceMetadata|InterfaceHashMismatch|hash mismatch|Failed to instantiate|unknown import|Failed to start actor|permission-denied'
fail=0

verify() {
  local actor="$1" src="$2"
  local wasm="$PWD/$RESULT/${actor}.wasm"
  if [ ! -f "$wasm" ]; then echo "SPAWN-VERIFY FAIL: $actor (missing $wasm)"; return 1; fi
  local man="$work/${actor}.toml"
  sed -e "s|^package = .*|package = \"$wasm\"|" \
      -e "s|^base_path = .*|base_path = \"$store\"|" \
      "$src" > "$man"
  local out
  out=$(timeout 40 theater setup "$man" 2>&1 || true)
  if printf '%s' "$out" | grep -qiE "$SIG"; then
    echo "SPAWN-VERIFY FAIL: $actor"; printf '%s\n' "$out"; return 1
  fi
  echo "OK (set up clean): $actor"
}

verify inbox_acceptor       acceptor/manifest.toml       || fail=1
verify inbox_api_handler    api-handler/manifest.toml    || fail=1
verify inbox_mailbox        mailbox/manifest.toml        || fail=1
verify inbox_mailbox_router mailbox-router/manifest.toml || fail=1
verify inbox_smtp_acceptor  smtp-acceptor/manifest.toml  || fail=1
verify inbox_smtp_handler   smtp-handler/manifest.toml   || fail=1
verify inbox_cli            cli/manifest.toml            || fail=1

echo "----"
if [ "$fail" = 0 ]; then echo "spawn-verify: all composites set up clean"; else echo "spawn-verify: FAILURES above"; fi
exit "$fail"
