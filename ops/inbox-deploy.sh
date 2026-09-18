#!/bin/sh
# inbox self-service deploy (piece 4) -- option B: store-publishd HTTPS interface.
#
# Flow: for each actor, POST the wasm to store-publishd (token-authed) and confirm
# the returned content-hash == our local sha256 (integrity over the HTTPS POST) ->
# optionally poll GET /resolve until the label converges cluster-wide -> restart
# the acceptor (in-process, :9000 control plane) so the tree re-reads its label
# symlinks fresh (static_package=false) -> VERIFY it took, fail-loud:
#   (1) current_id rotated  = the tree actually respawned   (supervisor status)
#   (2) /version == build   = the NEW code loaded, not stale (api-handler attest)
# Neither alone is enough; together they can't report "deployed" while stale.
#
# Pin before use (curl'd into the container): store-publishd not run locally --
# it's VPS-side; we only talk to it over pinned TLS (--cacert cert.pem). The wasm
# comes from CI; the store's content hash IS sha256 (store-dev id=630).
#
# set -u (undefined-var guard) but NOT set -e: the /resolve convergence poll
# EXPECTS transient non-zero (404 until the event folds cluster-wide). Every
# critical command has an explicit || fail.
set -u

BUILD_ID="${1:?usage: inbox-deploy.sh <build-id> <wasm-dir>}"   # git sha/tag CI baked into the wasm (/version)
WASM_DIR="${2:?dir holding the 6 CI-built inbox_<actor>.wasm}"

# --- inbox control plane + /version (interfaces I already have) ---
PROFILE="${SUPERVISOR_PROFILE:-prod}"
HANDLE="${INBOX_HANDLE:-inbox}"
API="${INBOX_API_URL:-https://mail.colinrozzi.com}"
TOKEN="$(cat "${INBOX_TOKEN_FILE:-/root/.config/inbox/token}")"     # mail API bearer, for /version
ACTORS_JSON="${INBOX_ACTORS_JSON:-ops/inbox-actors.json}"

# --- store-publishd (option B). Defaults = inbox-dev's container mount / the live
#     endpoint; override via env for another deployer/environment. ---
PUBLISHD="${STORE_PUBLISHD_URL:-https://mail.colinrozzi.com:18443}"
PUBLISH_TOKEN="$(cat "${STORE_PUBLISH_TOKEN_FILE:-/root/.config/inbox/publish-token}")"
PUBLISH_CACERT="${STORE_PUBLISH_CACERT:-/root/.config/inbox/store-publishd-cert.pem}"

log()  { printf '[inbox-deploy] %s\n' "$*"; }
fail() { printf '[inbox-deploy] FAIL: %s\n' "$*" >&2; exit 1; }
sha256_of() { sha256sum "$1" | awk '{print $1}'; }

# ---- 1. PUBLISH (POST raw wasm; verify returned hash == local sha256) ----
publish() {
  jq -r '.[] | .wasm' "$ACTORS_JSON" | while read -r label; do
    f="$WASM_DIR/${label}.wasm"
    [ -f "$f" ] || fail "missing wasm $f"
    log "publish $label <- $f"
    resp="$(curl -fsS --max-time 60 --cacert "$PUBLISH_CACERT" \
              -X POST "$PUBLISHD/publish?name=$label" \
              -H "Authorization: Bearer $PUBLISH_TOKEN" \
              --data-binary @"$f")" || fail "POST /publish $label"
    got="$(printf '%s' "$resp" | jq -r '.hash // empty')"
    want="$(sha256_of "$f")"
    [ -n "$got" ] || fail "publishd returned no hash for $label (resp: $resp)"
    [ "$got" = "$want" ] || fail "INTEGRITY: publishd hash $got != local sha256 $want for $label"
    log "published $label hash=$got (integrity ok)"
  done
}

# ---- 2. CONVERGENCE gate (poll /resolve until the label folds cluster-wide) ----
# POST 200 already means authored+folded on the co-located peer; this confirms the
# gossiped value is visible before we restart, so we never restart into a stale
# symlink. 404 during convergence is expected (empty -> keep polling).
await_convergence() {
  jq -r '.[] | .wasm' "$ACTORS_JSON" | while read -r label; do
    want="$(sha256_of "$WASM_DIR/${label}.wasm")"
    i=0
    while [ "$i" -lt 30 ]; do
      got="$(curl -fsS --max-time 15 --cacert "$PUBLISH_CACERT" \
               "$PUBLISHD/resolve?name=$label" \
               -H "Authorization: Bearer $PUBLISH_TOKEN" 2>/dev/null | tr -d '[:space:]')"
      [ "$got" = "$want" ] && break
      i=$((i+1)); sleep 2
    done
    [ "$got" = "$want" ] || fail "label $label did not converge (resolve=$got want=$want)"
    log "converged $label -> $want"
  done
}

# ---- 3. RELOAD + VERIFY (control plane + /version; unchanged from the design) ----
status_current_id() { supervisor status "$HANDLE" --profile "$PROFILE" 2>/dev/null | jq -r '.service.current_id // empty'; }
running_build_id()  { curl -fsS --max-time 15 -H "Authorization: Bearer $TOKEN" "$API/version" 2>/dev/null | jq -r '.build_id // empty'; }

reload_and_verify() {
  old_id="$(status_current_id)"; [ -n "$old_id" ] || fail "no current_id pre-restart (control plane?)"
  log "pre-restart current_id=$old_id"
  supervisor restart "$HANDLE" --profile "$PROFILE" || fail "supervisor restart"

  i=0; new_id=""
  while [ "$i" -lt 30 ]; do
    new_id="$(status_current_id)"
    [ -n "$new_id" ] && [ "$new_id" != "$old_id" ] && break
    i=$((i+1)); sleep 2
  done
  [ -n "$new_id" ] && [ "$new_id" != "$old_id" ] || fail "current_id did not rotate ($old_id) -> no respawn"
  log "respawned: current_id $old_id -> $new_id"

  i=0; got=""
  while [ "$i" -lt 15 ]; do
    got="$(running_build_id)"; [ -n "$got" ] && break
    i=$((i+1)); sleep 2
  done
  [ -n "$got" ] || fail "/version unreachable after restart"
  [ "$got" = "$BUILD_ID" ] || fail "STALE DEPLOY: /version build_id=$got != published $BUILD_ID"
  log "verified running build_id=$got == published $BUILD_ID"
}

publish
await_convergence
reload_and_verify
log "DEPLOY OK: inbox at build $BUILD_ID, respawned + attested (no stale)."
