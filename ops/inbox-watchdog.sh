#!/usr/bin/env bash
#
# inbox-watchdog.sh — liveness watchdog for the inbox mail spine.
#
# Runs on the VPS on a periodic systemd timer (inbox-watchdog.timer ->
# inbox-watchdog.service). Each invocation runs one probe cycle and, after
# 2 CONSECUTIVE failures of a given probe, restarts inbox.service.
#
# PROBES
#   1. READ  — GET the local read endpoint. Any HTTP response (a non-empty
#              status code that is not 000) means the accept/read path is
#              alive; a hang / no response means it is wedged.
#   2. SEND  — POST /send for a self-addressed loopback probe. This exercises
#              the full outbound path (api-handler -> router -> SMTP loopback).
#              A timeout / connection-hang / 0-byte response is the
#              "delivers-but-hangs" wedge we hit and must catch. Added because
#              the read probe alone does NOT cover the send path — a wedged
#              /send slips past a read-only watchdog.
#
# WHY TWO SEPARATE FAILCOUNTERS (do not "simplify" into one):
#   The send-wedge slipped past the old watchdog precisely because a healthy
#   read cleared the shared health state. Read and send therefore keep
#   INDEPENDENT consecutive-failure counters. A healthy read must NOT clear a
#   pending send-wedge, and vice-versa. A restart fires when EITHER counter
#   reaches the threshold; the restart then clears BOTH.
#
# GOVERNANCE: this script is version-controlled here and deployed to
#   /usr/local/bin/inbox-watchdog.sh on the VPS by the manager (manager-gated).
#   The inbox-watchdog.service / .timer units live on the VPS.
#
# >>> RECONSTRUCTION NOTE <<<
#   The read-probe half is reconstructed from the described behavior of the
#   live VPS script (which is not in this repo). Before deploying, DIFF this
#   against the live /usr/local/bin/inbox-watchdog.sh and reconcile the exact
#   READ_URL, curl flags, and TOKEN_FILE path. The send-probe half is the new
#   logic this change adds.

set -u

# ----------------------------------------------------------------------------
# Config — confirm these against the live VPS before deploy.
# ----------------------------------------------------------------------------
SERVICE="${INBOX_SERVICE:-inbox.service}"
FAIL_THRESHOLD="${INBOX_FAIL_THRESHOLD:-2}"   # consecutive failures -> restart

# Read probe: any HTTP response = alive. No auth required.
READ_URL="${INBOX_READ_URL:-https://127.0.0.1:443/}"
READ_TIMEOUT="${INBOX_READ_TIMEOUT:-10}"      # seconds, hard client-side cap

# Send probe: self-addressed loopback so we exercise the real send path
# without emitting external mail. The probe address must be registered.
PROBE_ADDR="${INBOX_PROBE_ADDR:-watchdog-probe@colinrozzi.com}"
SEND_URL="${INBOX_SEND_URL:-https://127.0.0.1:443/v1/mailboxes/${PROBE_ADDR}/send}"
SEND_TIMEOUT="${INBOX_SEND_TIMEOUT:-10}"      # seconds, hard client-side cap
# /send requires a bearer token (unlike the read probe). Source it from wherever
# the VPS keeps the inbox bearer token — CONFIRM this path on the VPS.
TOKEN_FILE="${INBOX_TOKEN_FILE:-/var/lib/inbox/token}"

# Per-probe consecutive-failure state (survives across timer invocations).
READ_FAILCOUNT="${INBOX_READ_FAILCOUNT:-/run/inbox-watchdog.failcount}"
SEND_FAILCOUNT="${INBOX_SEND_FAILCOUNT:-/run/inbox-watchdog.send.failcount}"

# ----------------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------------
log() { logger -t inbox-watchdog "$*"; }

read_count() {  # $1 = failcount file -> prints current count (0 if absent)
  local n
  n=$(cat "$1" 2>/dev/null || echo 0)
  case "$n" in
    ''|*[!0-9]*) echo 0 ;;
    *)           echo "$n" ;;
  esac
}

clear_count() { rm -f "$1" 2>/dev/null || true; }

# bump <failcount-file> <probe-label> -> increments, logs, returns new count
bump() {
  local file="$1" label="$2" n
  n=$(read_count "$file")
  n=$((n + 1))
  echo "$n" > "$file"
  log "$label probe FAILED (${n}/${FAIL_THRESHOLD} consecutive)"
  echo "$n"
}

# ----------------------------------------------------------------------------
# READ probe — any HTTP status code that is not empty and not 000 = alive.
# ----------------------------------------------------------------------------
read_code=$(curl -sk -o /dev/null -w '%{http_code}' \
  --max-time "$READ_TIMEOUT" "$READ_URL" 2>/dev/null || echo "000")

if [ -n "$read_code" ] && [ "$read_code" != "000" ]; then
  clear_count "$READ_FAILCOUNT"
else
  bump "$READ_FAILCOUNT" "read" >/dev/null
fi

# ----------------------------------------------------------------------------
# SEND probe — POST /send for the loopback probe address. Healthy = a non-empty
# HTTP response within the timeout. A timeout / connection-hang / 0-byte body
# (the delivers-but-hangs wedge) = failure.
# ----------------------------------------------------------------------------
if [ -r "$TOKEN_FILE" ]; then
  token=$(cat "$TOKEN_FILE" 2>/dev/null)
else
  token=""
  log "send probe: token file '$TOKEN_FILE' not readable — treating send probe as failure"
fi

if [ -n "$token" ]; then
  # -w captures "<http_code> <size_download>"; on timeout/hang curl exits
  # non-zero and we fall back to the sentinel "000 0".
  send_out=$(curl -sk -o /dev/null -w '%{http_code} %{size_download}' \
    --max-time "$SEND_TIMEOUT" \
    -X POST "$SEND_URL" \
    -H "Authorization: Bearer ${token}" \
    -H "Content-Type: application/json" \
    -d "{\"to\":[\"${PROBE_ADDR}\"],\"subject\":\"inbox-watchdog send probe\",\"body\":\"liveness probe — self-addressed loopback\"}" \
    2>/dev/null || echo "000 0")
  send_code="${send_out%% *}"
  send_size="${send_out##* }"
else
  send_code="000"
  send_size="0"
fi

# Wedge = no HTTP response (000/empty) OR a 0-byte body (delivers-but-hangs).
if [ -n "$send_code" ] && [ "$send_code" != "000" ] && [ "${send_size:-0}" -gt 0 ] 2>/dev/null; then
  clear_count "$SEND_FAILCOUNT"
else
  bump "$SEND_FAILCOUNT" "send" >/dev/null
fi

# ----------------------------------------------------------------------------
# Remediation — restart if EITHER probe has reached the threshold, then clear
# BOTH counters so we start clean after the restart.
# ----------------------------------------------------------------------------
read_n=$(read_count "$READ_FAILCOUNT")
send_n=$(read_count "$SEND_FAILCOUNT")

if [ "$read_n" -ge "$FAIL_THRESHOLD" ] || [ "$send_n" -ge "$FAIL_THRESHOLD" ]; then
  log "restarting ${SERVICE} after ${FAIL_THRESHOLD} consecutive failures (read=${read_n} send=${send_n})"
  systemctl restart "$SERVICE"
  clear_count "$READ_FAILCOUNT"
  clear_count "$SEND_FAILCOUNT"
fi

exit 0
