#!/usr/bin/env bash
# public-instance-cap-lock-proof.sh — proves the T0 capability boundary that makes
# the inbox safe to hand real tokens to UNTRUSTED customers (the public product).
#
# The headline invariant: "creating/claiming an address requires the `admin`
# capability; a `use`-only customer token cannot — and the operator/admin creds
# never reach a customer." On the public (company) instance this is UNCONDITIONAL:
# the legacy shared-bearer path (whose `Caller::Legacy` has EVERY capability) is
# hard-refused, so there is no `enforce=off => all caps` foot-gun.
#
# Complements ops/two-tenant-proof.sh: that proves cross-tenant isolation using
# ADMIN (root) tokens (claiming a taken/reserved address -> 409). THIS proves the
# CAP boundary with a USE-ONLY token (any register attempt -> 403) and the
# public-instance legacy refusal (-> 401).
#
# ── WHERE THIS RUNS ──────────────────────────────────────────────────────────
# Assertions are pure curl (run anywhere the enforcing public instance is live).
# SETUP needs theater access (mint the keys).
#
# ── SETUP (where theater + the new wasm live) ────────────────────────────────
#   1. Spawn the inbox enforcing: store label `tenancy-enforce` = "1", registry wired.
#   2. Set store label `public-instance` = "1"  (NEW — turns on the legacy refusal).
#   3. Create a tenant and mint TWO keys for it:
#        - TOKEN_ADMIN: an `admin`-capped key (registry.mint-key tenant ["use","admin"] ...)
#        - TOKEN_USE:   a `use`-only key     (registry.mint-key tenant ["use"] ...)
#      Register the tenant's mailbox with TOKEN_ADMIN (admin stamps ownership):
#        curl -X POST "$INBOX_API/v1/mailboxes" -H "Authorization: Bearer $TOKEN_ADMIN" \
#             -H 'Content-Type: application/json' -d "{\"address\":\"$ADDR_OWN\"}"
#   4. Export TOKEN_ADMIN, TOKEN_USE, ADDR_OWN, and optionally LEGACY_BEARER (the old
#      shared api-bearer-token, if one exists on the box) to prove it's refused.
#
# ── RUN ──────────────────────────────────────────────────────────────────────
#   INBOX_API=https://agent-inbox.dev \
#   TOKEN_ADMIN=... TOKEN_USE=... ADDR_OWN=a7f3k2@agent-inbox.dev \
#   ADDR_OTHER=zzz999@agent-inbox.dev LEGACY_BEARER=... \
#   bash ops/public-instance-cap-lock-proof.sh
#
# Exit 0 iff every cap-boundary assertion holds.
set -uo pipefail

API="${INBOX_API:-https://agent-inbox.dev}"
: "${TOKEN_ADMIN:?set TOKEN_ADMIN (an admin-capped key)}"
: "${TOKEN_USE:?set TOKEN_USE (a use-only key, same tenant)}"
: "${ADDR_OWN:?set ADDR_OWN (the tenant's registered address)}"
ADDR_OTHER="${ADDR_OTHER:-nobody-else-$RANDOM@agent-inbox.dev}"
NEW_ADDR="${NEW_ADDR:-claim-attempt-$RANDOM@agent-inbox.dev}"

pass=0
fail=0

# status <label> <want-code> <token> <method> <path> [body]
status() {
  local label="$1" want="$2" tok="$3" method="$4" path="$5" body="${6:-}"
  local args=(-sS -o /dev/null -w '%{http_code}' -X "$method" "$API$path" -H "Authorization: Bearer $tok")
  [ -n "$body" ] && args+=(-H 'Content-Type: application/json' -d "$body")
  local got
  got=$(curl "${args[@]}" 2>/dev/null || echo "ERR")
  if [ "$got" = "$want" ]; then
    printf 'PASS  %-52s %s\n' "$label" "$got"
    pass=$((pass + 1))
  else
    printf 'FAIL  %-52s got %s, want %s\n' "$label" "$got" "$want"
    fail=$((fail + 1))
  fi
}

echo "== use-only token: the 'use' cap works on its OWN mailbox =="
status "USE reads own inbox            -> 200" 200 "$TOKEN_USE" GET  "/v1/mailboxes/$ADDR_OWN/inbox?since=0"
status "USE sends from own address     -> 200" 200 "$TOKEN_USE" POST "/v1/mailboxes/$ADDR_OWN/send" "{\"to\":[\"$ADDR_OWN\"],\"subject\":\"self\",\"body\":\"ok\"}"

echo "== CAP BOUNDARY: use-only token CANNOT create/claim any address (403, not 409) =="
# 403 = capability refused (admin required), distinct from 409 = address unavailable.
# A use-only customer must be stopped at the CAP, before any address-availability check.
status "USE registers a fresh address  -> 403" 403 "$TOKEN_USE" POST "/v1/mailboxes" "{\"address\":\"$NEW_ADDR\"}"
status "USE re-registers OWN address   -> 403" 403 "$TOKEN_USE" POST "/v1/mailboxes" "{\"address\":\"$ADDR_OWN\"}"

echo "== use-only token: no cross-mailbox reach (404, never 403/existence-leak) =="
status "USE reads a foreign mailbox    -> 404" 404 "$TOKEN_USE" GET  "/v1/mailboxes/$ADDR_OTHER/inbox?since=0"
status "USE sends AS a foreign address -> 404" 404 "$TOKEN_USE" POST "/v1/mailboxes/$ADDR_OTHER/send" '{"to":["x@y.z"],"subject":"x","body":"x"}'

echo "== admin cap still works (control-plane path) =="
status "ADMIN registers a new address  -> 201" 201 "$TOKEN_ADMIN" POST "/v1/mailboxes" "{\"address\":\"$NEW_ADDR\"}"

echo "== PUBLIC-INSTANCE: the legacy shared-bearer path is hard-refused =="
# With public-instance=1, authenticate() never yields Caller::Legacy. A legacy shared
# bearer (if one even exists on the box) must be rejected like any unknown token.
if [ -n "${LEGACY_BEARER:-}" ]; then
  status "legacy shared bearer          -> 401" 401 "$LEGACY_BEARER" GET "/v1/mailboxes/$ADDR_OWN/inbox?since=0"
else
  echo "SKIP  legacy shared bearer (set LEGACY_BEARER to assert 401)"
fi
status "unknown token                  -> 401" 401 "not-a-real-key" GET "/v1/mailboxes/$ADDR_OWN/inbox?since=0"

echo
echo "==== $pass passed, $fail failed ===="
[ "$fail" -eq 0 ] || { echo "CAP-LOCK PROOF FAILED"; exit 1; }
echo "CAP-LOCK PROOF GREEN"
