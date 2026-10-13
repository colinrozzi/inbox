#!/usr/bin/env bash
# two-tenant-proof.sh — the multi-tenant ISOLATION proof.
#
# Proves tenant A cannot reach tenant B's mailbox across EVERY API surface. This is
# (a) the runtime validation of the enforcing path — CI's spawn-verify runs with
# enforcement OFF, so the authorize()/registry.resolve/lookup-owned path is never
# otherwise exercised — and (b) the concrete live target of the adversarial
# isolation review.
#
# ── WHERE THIS RUNS ──────────────────────────────────────────────────────────
# NOT in the inbox-dev container (no theater, no toolchain). Run it on the dev box
# / anywhere the enforcing inbox is live. The ASSERTIONS below are pure curl and
# run anywhere; only the SETUP needs theater access.
#
# ── SETUP (do this first, where theater + the new wasm live) ─────────────────
#   1. Spawn the inbox with the acceptor initial_state carrying
#      `tenant_registry_manifest` (so the registry singleton comes up).
#   2. Seed store label `tenant-registry-seed` with high-entropy bytes.
#   3. Set store label `tenancy-enforce` = "1".
#   4. Create two tenants via the registry (registry.create-tenant "A" / "B") and
#      capture each returned root_token. (No HTTP create-tenant route yet — RPC the
#      registry actor directly, or add the control-plane routes first.) Export them
#      as TOKEN_A / TOKEN_B.
#   5. Each tenant registers ITS OWN mailbox through the API with its root token —
#      this exercises register-owned (admin cap stamps ownership):
#        curl -X POST "$INBOX_API/v1/mailboxes" -H "Authorization: Bearer $TOKEN_A" \
#             -H 'Content-Type: application/json' -d "{\"address\":\"$ADDR_A\"}"
#        curl -X POST "$INBOX_API/v1/mailboxes" -H "Authorization: Bearer $TOKEN_B" \
#             -H 'Content-Type: application/json' -d "{\"address\":\"$ADDR_B\"}"
#
# ── RUN ──────────────────────────────────────────────────────────────────────
#   INBOX_API=https://mail.colinrozzi.com \
#   TOKEN_A=... TOKEN_B=... ADDR_A=a@colinrozzi.com ADDR_B=b@colinrozzi.com \
#   RESERVED=postmaster@colinrozzi.com \
#   [PUBLIC_INSTANCE=1]   # set when the instance runs public-instance=1 (the public product)
#   bash ops/two-tenant-proof.sh
#
# Exit 0 iff every isolation assertion holds. Mail-bearing cross-tenant checks demand
# 404 (NOT 403) — a mailbox you don't control must be indistinguishable from absent.
# GET <addr> (info/existence) is model-dependent: PUBLIC_INSTANCE=1 => 404 (existence-
# privacy, T0 invariant); unset (fleet) => 200 (global discovery, matches send-routing).
set -uo pipefail

API="${INBOX_API:-https://mail.colinrozzi.com}"
: "${TOKEN_A:?set TOKEN_A (tenant A root token)}"
: "${TOKEN_B:?set TOKEN_B (tenant B root token)}"
: "${ADDR_A:?set ADDR_A (tenant A address)}"
: "${ADDR_B:?set ADDR_B (tenant B address)}"
RESERVED="${RESERVED:-postmaster@colinrozzi.com}"
BAD="${TOKEN_BAD:-not-a-real-key}"
# PUBLIC_INSTANCE: set (="1") to validate the public-instance isolation model (cross-
# tenant info -> 404); unset to validate the cooperative fleet model (info -> 200 discovery).

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
    printf 'PASS  %-46s %s\n' "$label" "$got"
    pass=$((pass + 1))
  else
    printf 'FAIL  %-46s got %s, want %s\n' "$label" "$got" "$want"
    fail=$((fail + 1))
  fi
}

# body_excludes <label> <token> <path> <needle> : list must NOT contain needle
body_excludes() {
  local label="$1" tok="$2" path="$3" needle="$4"
  local out
  out=$(curl -sS "$API$path" -H "Authorization: Bearer $tok" 2>/dev/null || echo "")
  if printf '%s' "$out" | grep -q -- "$needle"; then
    printf 'FAIL  %-46s leaked %s\n' "$label" "$needle"
    fail=$((fail + 1))
  else
    printf 'PASS  %-46s (no %s)\n' "$label" "$needle"
    pass=$((pass + 1))
  fi
}

echo "== own access works (sanity) =="
status "A reads own inbox"                200 "$TOKEN_A" GET  "/v1/mailboxes/$ADDR_A/inbox?since=0"
status "A sees own mailbox info"          200 "$TOKEN_A" GET  "/v1/mailboxes/$ADDR_A"
status "B reads own inbox"                200 "$TOKEN_B" GET  "/v1/mailboxes/$ADDR_B/inbox?since=0"

echo "== CROSS-TENANT: A must NOT reach B (404, never 403) =="
# Mail-bearing routes are owner-gated on EVERY instance — discovering a peer must
# never grant reading its mail, sending as it, or backfilling it. These are 404 always.
status "A reads B inbox -> 404"           404 "$TOKEN_A" GET  "/v1/mailboxes/$ADDR_B/inbox?since=0"
status "A sends AS B    -> 404"           404 "$TOKEN_A" POST "/v1/mailboxes/$ADDR_B/send" '{"to":["x@y.z"],"subject":"x","body":"x"}'
status "A backfills B   -> 404"           404 "$TOKEN_A" POST "/v1/mailboxes/$ADDR_B/backfill-raw"
status "B reads A inbox -> 404"           404 "$TOKEN_B" GET  "/v1/mailboxes/$ADDR_A/inbox?since=0"

# GET <addr> (info/existence) is MODEL-DEPENDENT (the send/lookup-asymmetry fix):
#   PUBLIC instance (PUBLIC_INSTANCE=1): owner-isolated — cross-tenant info -> 404
#     (existence-privacy; the T0 invariant a customer must not probe another's address).
#   FLEET instance (unset): global discovery — cross-tenant info -> 200 (resolves any
#     registered address + its mailbox_id, matching send-routing) so agents pre-flight peers.
# Asserting BOTH here is the safety that makes the fix landable.
if [ -n "${PUBLIC_INSTANCE:-}" ]; then
  status "A reads B info  -> 404 (public: existence-private)"  404 "$TOKEN_A" GET "/v1/mailboxes/$ADDR_B"
else
  status "A reads B info  -> 200 (fleet: discovery resolves)"  200 "$TOKEN_A" GET "/v1/mailboxes/$ADDR_B"
fi

echo "== list is tenant-scoped (no cross-tenant existence leak) =="
body_excludes "A list omits B address"    "$TOKEN_A" "/v1/mailboxes" "$ADDR_B"
body_excludes "B list omits A address"    "$TOKEN_B" "/v1/mailboxes" "$ADDR_A"

echo "== register: can't claim another tenant's address, nor a reserved one =="
status "A re-claims B addr -> 409"        409 "$TOKEN_A" POST "/v1/mailboxes" "{\"address\":\"$ADDR_B\"}"
status "A claims reserved  -> 409"        409 "$TOKEN_A" POST "/v1/mailboxes" "{\"address\":\"$RESERVED\"}"

echo "== FIX 1 regression: a case-variant of another tenant's address must COLLIDE, not twin =="
# Pre-FIX-1 this returned 201 (a second Binding{tenant:B} + mailbox — the exact
# cross-tenant hole the review found). After FIX 1 the router canonicalizes, so B
# claiming a mis-cased variant of A's address collides on the one-tenant invariant.
ADDR_A_CV="${ADDR_A_CV:-$(printf '%s' "$ADDR_A" | tr '[:lower:]' '[:upper:]')}"
status "B claims A addr mis-cased -> 409" 409 "$TOKEN_B" POST "/v1/mailboxes" "{\"address\":\"$ADDR_A_CV\"}"
# NOT curl-testable here (SMTP): after FIX 1, inbound RCPT to a mis-cased variant of
# A's address must resolve to A's single canonical mailbox, never a B twin. Verify
# with an SMTP send to the mis-cased address on the dev box.

echo "== auth: unknown token is rejected =="
status "bad token -> 401"                 401 "$BAD"     GET  "/v1/mailboxes/$ADDR_A/inbox?since=0"
status "no-address list w/ bad token 401" 401 "$BAD"     GET  "/v1/mailboxes"

echo
echo "==== $pass passed, $fail failed ===="
[ "$fail" -eq 0 ] || { echo "ISOLATION PROOF FAILED"; exit 1; }
echo "ISOLATION PROOF GREEN"
