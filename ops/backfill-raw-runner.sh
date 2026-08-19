#!/usr/bin/env bash
#
# backfill-raw-runner.sh — one-shot sweep to give every already-stored message a
# raw_ref. Lists the registered mailboxes (GET /v1/mailboxes) and POSTs
# .../backfill-raw to each. Idempotent per mailbox — the mailbox skips messages
# that already have a ref — so it is safe to re-run and safe to interrupt.
#
# Run ONCE after the raw-retention deploy (PR #70). New mail already carries its
# real raw; this fills a reconstructed raw for messages received before the
# deploy (marked X-Inbox-Reconstructed: 1; lossy — original attachments/HTML for
# those are gone). Resolving each address spawns its mailbox if needed, so a big
# mailbox's first sweep may take a moment.
#
# Requires: curl, jq.
#   INBOX_BASE        default https://127.0.0.1:443     (VPS loopback)
#   INBOX_TOKEN_FILE  default /var/lib/inbox/token       (bearer token)
#   INBOX_CURL_OPTS   default -sk                        (-k for the self-signed loopback cert)
set -u

BASE="${INBOX_BASE:-https://127.0.0.1:443}"
TOKEN_FILE="${INBOX_TOKEN_FILE:-/var/lib/inbox/token}"
CURL_OPTS="${INBOX_CURL_OPTS:--sk}"

command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
[ -r "$TOKEN_FILE" ] || { echo "token file '$TOKEN_FILE' not readable" >&2; exit 2; }
TOKEN="$(cat "$TOKEN_FILE")"

addrs=$(curl $CURL_OPTS --max-time 20 "$BASE/v1/mailboxes" \
  -H "Authorization: Bearer $TOKEN" | jq -r '.mailboxes[]?') \
  || { echo "failed to list mailboxes" >&2; exit 1; }
[ -n "$addrs" ] || { echo "no mailboxes found" >&2; exit 1; }

total=0; n_mbx=0; n_fail=0
tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT

while IFS= read -r addr; do
  [ -n "$addr" ] || continue
  n_mbx=$((n_mbx + 1))
  enc=$(printf '%s' "$addr" | jq -sRr '@uri')
  code=$(curl $CURL_OPTS --max-time 180 -o "$tmp" -w '%{http_code}' -X POST \
    "$BASE/v1/mailboxes/$enc/backfill-raw" -H "Authorization: Bearer $TOKEN" \
    2>/dev/null || echo 000)
  if [ "$code" = "200" ]; then
    filled=$(jq -r '.backfilled // 0' "$tmp" 2>/dev/null || echo 0)
    total=$((total + filled))
    printf 'OK    %-42s backfilled=%s\n' "$addr" "$filled"
  else
    n_fail=$((n_fail + 1))
    printf 'FAIL  %-42s http=%s %s\n' "$addr" "$code" "$(head -c 200 "$tmp")"
  fi
done <<< "$addrs"

echo "----"
echo "swept $n_mbx mailboxes · backfilled $total messages · $n_fail failures"
[ "$n_fail" -eq 0 ]
