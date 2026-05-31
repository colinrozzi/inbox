# inbox deployment runbook

Real-internet deployment of the inbox: own domain, real MX, two-way SMTP with gmail. This is the path that was used to bring up `mail.colinrozzi.com`. Steps are ordered so each unblocks the next.

If you already have a domain, a VPS with rust or nix, and a clue, the whole thing is maybe 30 minutes of work — but it's spread across three providers (DNS, VPS, registrar/host) and the order matters.

## Prerequisites

- A VPS with a public IPv4 and an IPv6 (most cloud providers give you both). Root or `sudo`.
- A domain you control. The DNS for it should let you set A, AAAA, MX, TXT records; Cloudflare is what the reference deployment uses.
- Outbound port 25 not blocked at the VPS provider. **Most providers block :25 by default to prevent spam abuse — you typically have to open a support ticket to lift it.** A sample ticket is in `linode-smtp-ticket.md` (one directory up in the reference deployment).
- Either `nix` on the VPS (and locally), or a Rust toolchain with the `wasm32-unknown-unknown` target.

## 1. DNS

Set the following records in your zone. Replace `colinrozzi.com` with your domain, `45.33.64.210` with your VPS IPv4, `2600:3c03::f03c:94ff:fec6:6bb7` with your VPS IPv6.

| Type | Name | Value | Proxy/Cloud |
|---|---|---|---|
| A | `mail` | `45.33.64.210` | DNS-only (not proxied) |
| AAAA | `mail` | `2600:3c03::f03c:94ff:fec6:6bb7` | DNS-only |
| MX | `@` | `10 mail.colinrozzi.com` | — |
| TXT | `@` | `v=spf1 ip4:45.33.64.210 ip6:2600:3c03::f03c:94ff:fec6:6bb7 ~all` | — |
| TXT | `_dmarc` | `v=DMARC1; p=none; rua=mailto:colin@colinrozzi.com; aspf=r; adkim=r` | — |
| TXT | `default._domainkey` | (DKIM public key — see step 3) | — |

**Pitfalls:**
- Both A and AAAA on `mail.<domain>` must be DNS-only. SMTP traffic does not go through Cloudflare's proxy.
- Only **one** SPF record. Multiple records cause `permerror` and silently kill outbound deliverability.
- Include IPv6 in SPF if your VPS has one. Outbound to dual-stack receivers preferentially uses IPv6.

## 2. Reverse DNS (PTR)

Forward-confirmed reverse DNS (FCrDNS) is non-optional for getting mail into gmail. Both PTRs must resolve back to a hostname whose A/AAAA points to the same IP.

- **IPv4 PTR** for `45.33.64.210` → `mail.colinrozzi.com`. Set in the VPS provider panel (Linode: Network → IPv4 → ⋯ → Edit RDNS).
- **IPv6 PTR** for `2600:3c03::f03c:94ff:fec6:6bb7` → `mail.colinrozzi.com`. Same panel, IPv6 section. **Do not "Add an IP Address"** — you're editing the rDNS on the existing /128 you already have.

Verify (may take a minute to propagate):

```sh
dig +short -x 45.33.64.210                    # → mail.colinrozzi.com.
dig +short -x 2600:3c03::f03c:94ff:fec6:6bb7  # → mail.colinrozzi.com.
dig +short a    mail.colinrozzi.com           # → 45.33.64.210
dig +short aaaa mail.colinrozzi.com           # → 2600:3c03::f03c:94ff:fec6:6bb7
```

## 3. DKIM keypair

Generate on the VPS so the private key never leaves:

```sh
mkdir -p /etc/inbox/dkim
cd /etc/inbox/dkim
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out private.pem
chmod 600 private.pem
openssl pkey -in private.pem -pubout -out public.pem
```

Extract the public key as a single base64 line for the DNS TXT:

```sh
sed -n '/-----BEGIN PUBLIC KEY-----/,/-----END PUBLIC KEY-----/p' public.pem \
  | sed '1d;$d' | tr -d '\n'
```

Put that into your `default._domainkey` TXT as `v=DKIM1; k=rsa; p=<key>`. Most DNS UIs auto-chunk the long string across DKIM-compatible TXT-string segments.

## 4. Bearer token

The HTTP API is bearer-token-authed; without it, every route returns `401`. Generate a token once on the deploy host:

```sh
openssl rand -hex 32 > /etc/inbox/api-token
chmod 600 /etc/inbox/api-token
```

This token gets embedded in the acceptor manifest's `initial_state` (next step) and copied to each client machine that wants to talk to the API. On a client:

```sh
mkdir -p ~/.config/inbox
scp your-vps:/etc/inbox/api-token ~/.config/inbox/token
chmod 600 ~/.config/inbox/token
```

The CLI looks at `$INBOX_TOKEN` first, then `~/.config/inbox/token`. Curl from anywhere is `-H "Authorization: Bearer $(cat ~/.config/inbox/token)"`.

To rotate the bearer without a 401-flicker window: write `<new>,<old>` to the acceptor manifest's bearer line in `initial_state` (comma-separated), redeploy, distribute the new bearer to all consumers, then redeploy again with just `<new>`. The acceptor stores whatever line you give it; the api-handler accepts any comma-separated entry.

## 5. Build

Locally (recommended — the VPS may not have enough disk for a full rust build):

```sh
cd /path/to/inbox
nix build .#default .#theater
nix copy --no-check-sigs \
  --to "ssh-ng://your-vps?remote-program=/nix/var/nix/profiles/default/bin/nix-daemon" \
  ./result ./result-theater
```

Note the store paths — you'll wire them into the deployment manifests.

If you don't use nix: `cargo build --release --target wasm32-unknown-unknown`, scp the six `.wasm` files plus a theater binary up to the VPS.

## 6. Deployment manifests

The actor sources hardcode manifest paths under `/home/colin/work/actors/inbox/...`. On the deployment host, lay out the same directory structure and give each manifest the right `package = <nix-store-path>/inbox_<name>.wasm` line. The acceptor manifest also needs an `initial_state` field with the bearer token on the first line and the DKIM private key after it.

A working acceptor manifest looks like:

```toml
name = "inbox-acceptor"
version = "0.1.0"
package = "/nix/store/XXXX-inbox-0.1.0/inbox_acceptor.wasm"

initial_state = """\
<bearer-token>
-----BEGIN PRIVATE KEY-----
MIIEvQIB...
...
-----END PRIVATE KEY-----
"""

[[handler]]
type = "runtime"

[[handler]]
type = "tcp"

[[handler]]
type = "supervisor"

[[handler]]
type = "rpc"

[[handler]]
type = "store"
base_path = "/var/lib/inbox/store"
store_id = "inbox"
```

The mailbox, mailbox-router, and api-handler manifests also need the `store` handler entry (same `base_path` and `store_id`). The other manifests just need `package = ...` updated to the same nix-store path. See the canonical manifests in each actor's directory.

A small script that updates package paths to the new build:

```sh
NEW=/nix/store/XXXX-inbox-0.1.0
for d in acceptor api-handler mailbox mailbox-router smtp-acceptor smtp-handler; do
  case "$d" in
    acceptor)        f=inbox_acceptor.wasm ;;
    api-handler)     f=inbox_api_handler.wasm ;;
    mailbox)         f=inbox_mailbox.wasm ;;
    mailbox-router)  f=inbox_mailbox_router.wasm ;;
    smtp-acceptor)   f=inbox_smtp_acceptor.wasm ;;
    smtp-handler)    f=inbox_smtp_handler.wasm ;;
  esac
  sed -i "s|^package = .*|package = \"$NEW/$f\"|" "/home/colin/work/actors/inbox/$d/manifest.toml"
done
```

## 7. State directories + GC roots

The inbox uses a disk-backed store for mailbox state, the DKIM key, and the router's address→mailbox map:

```sh
mkdir -p /var/lib/inbox/store /var/lib/inbox/gc-roots /var/log/inbox
```

Pin the running build's nix store paths so `nix-collect-garbage` can't yank them out from under the live process:

```sh
NIX=/nix/var/nix/profiles/default/bin/nix-store
$NIX --add-root /var/lib/inbox/gc-roots/theater --indirect --realise /nix/store/XXXX-theater-0.3.9
$NIX --add-root /var/lib/inbox/gc-roots/inbox   --indirect --realise /nix/store/XXXX-inbox-0.1.0
```

## 8. systemd unit

```ini
# /etc/systemd/system/inbox.service
[Unit]
Description=Theater inbox actor system
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/var/lib/inbox/gc-roots/theater/bin/theater spawn /home/colin/work/actors/inbox/acceptor/manifest.toml
WorkingDirectory=/var/lib/inbox
Restart=on-failure
RestartSec=5
StandardOutput=append:/var/log/inbox/theater.log
StandardError=append:/var/log/inbox/theater.log
LimitNOFILE=65536
# Runs as root because the smtp-acceptor binds privileged :25.

[Install]
WantedBy=multi-user.target
```

Enable and start:

```sh
systemctl daemon-reload
systemctl enable --now inbox.service
ss -tlnp | grep -E ':(8080|25)\b'
# 0.0.0.0:8080  ← HTTP API
# 0.0.0.0:25    ← inbound SMTP
```

The systemd unit references the GC-rooted symlink rather than a direct `/nix/store/...` path. When you deploy a new build, point the symlink at the new store path (`ln -snf .../new-theater /var/lib/inbox/gc-roots/theater`) and `systemctl restart inbox`.

## 9. Smoke tests

Every route needs `Authorization: Bearer <token>`. Set up a shell shortcut:

```sh
TOKEN=$(cat /etc/inbox/api-token)        # on the deploy host
# (or: TOKEN=$(cat ~/.config/inbox/token) from a client machine,
#  pointing at https://mail.<yourdomain>:8080 instead of localhost)

H="Authorization: Bearer $TOKEN"
```

Then:

```sh
# Register an address
curl -H "$H" -X POST -H 'Content-Type: application/json' \
  -d '{"address":"colin@colinrozzi.com"}' \
  http://localhost:8080/v1/mailboxes

# Look it up
curl -H "$H" 'http://localhost:8080/v1/mailboxes/colin%40colinrozzi.com'

# Send to a real recipient (gmail-smtp-in is gmail's MX)
curl -H "$H" -X POST -H 'Content-Type: application/json' \
  -d '{"to":"you@gmail.com","subject":"hello","body":"hi from inbox","smtp_server":"gmail-smtp-in.l.google.com:25"}' \
  'http://localhost:8080/v1/mailboxes/colin%40colinrozzi.com/send'

# Reply from gmail, then read the inbox
curl -H "$H" 'http://localhost:8080/v1/mailboxes/colin%40colinrozzi.com/inbox?since=0'
```

Or use the CLI from any client machine with `~/.config/inbox/token`:

```sh
INBOX_API=mail.colinrozzi.com:8080 ./cli/inbox list
INBOX_API=mail.colinrozzi.com:8080 ./cli/inbox send colin@colinrozzi.com \
  --to you@gmail.com --subject hello --body "hi from inbox"
```

## 10. Common failure modes

| Symptom | Cause | Fix |
|---|---|---|
| `connect to ... failed: Connection refused` for any outbound :25 | VPS provider blocks outbound SMTP | Open support ticket asking for :25 unblock |
| Gmail bounces with `550-5.7.25 ... does not have a PTR record` | rDNS missing on the sending IP | Set PTR in VPS provider's networking panel; check **which IP** gmail saw — likely IPv6 if you have AAAA |
| Gmail bounces with `550-5.7.26 ... SPF check failed` | SPF doesn't list the sending IP, or there are multiple SPF records | Single SPF including all sending IPs (`ip4:... ip6:...`) |
| DKIM verifier rejects | Mismatched key between `private.pem` and DNS TXT, or canonicalization differs | Re-extract public from the deployed private; `dig +short txt default._domainkey.<domain>` and compare |
| `Timeout waiting for actor runtime ... (10s)` after a failed send | api-handler shutdown stalls; subsequent sends queue up | Restart theater for now (see also TODOs in the inbox README) |
| `now()` hangs from a pack actor | `theater:simple/timer.now()` doesn't wire up for spawned pack actors | Don't depend on it; receivers will add `Date` if absent |
| Every request returns `{"error":"unauthorized"}` | Bearer token missing, doesn't match, or has trailing newline from a heredoc | `cat /etc/inbox/api-token` and compare to the token in `acceptor/manifest.toml` `initial_state`. `openssl rand -hex 32` output has a trailing newline — strip it. |

## 11. Pending hardening

- The bearer token flies in cleartext over plain HTTP. Fine for a trial deployment; for production add TLS (Caddy on `:443` → `:8080`, or theater's own TLS support if you're feeling adventurous).
- One shared token authorizes every route. Per-mailbox tokens (and per-mailbox owners) are the obvious next step.
- DMARC is `p=none`. Bump to `quarantine` or `reject` once you've seen a few days of clean aggregate reports in the rua mailbox.
- Under heavy concurrent load (>10–20 requests in a burst), individual TCP connections can fail with `Connection not found` — the acceptor logs each one and keeps running, but those requests are lost. Root cause is per-connection wasm-spawn cost serializing through one accept loop; fixes are an api-handler pool or a single long-lived api-handler.
- Theater's `timer.now()` doesn't currently work from pack actors, so outbound mail has no `Date` or `Message-ID` header; receivers add `Date` for us. Untracked theater bug.

## 12. Sentinel-managed deploy (cutover from systemd)

Sections 1-9 describe the systemd-managed deploy that's the legacy path. Inbox was cut over to sentinel-managed deploy on 2026-05-31 (release tag `release-20260531-d3b180d`); this section is the operational procedure for that flow.

### Prereqs

- sentinel running on the VPS with Phase 3.3 or later (multi-actor supervision + per-child secret-validation)
- Cert pair at `/etc/letsencrypt/live/mail.colinrozzi.com/{fullchain,privkey}.pem` (certbot-owned, renewed automatically)
- Existing store at `/mnt/main-volume/inbox/store` (sentinel-managed deploy reuses the systemd-deploy's store — zero migration of mailbox data)

### Per-deploy procedure

```sh
# 1. Cut a release tag against the desired inbox/main commit.
#    Triggers .github/workflows/release.yml — uploads 7 wasms + 5
#    sub-manifest TOMLs + tarball + sha256 as release assets.
git tag release-$(date +%Y%m%d)-$(git rev-parse --short=7 HEAD) origin/main
git push origin <tag>
# Wait ~3-5 min for workflow to complete.

# 2. Verify both TLS-bearing sub-manifests rendered correctly.
TAG=release-YYYYMMDD-<sha7>
curl -sL https://github.com/colinrozzi/inbox/releases/download/$TAG/inbox-api-handler-${TAG#release-}.toml | grep client_tls
curl -sL https://github.com/colinrozzi/inbox/releases/download/$TAG/inbox-smtp-handler-${TAG#release-}.toml | grep server_tls
# Both must return a matching line. If either is empty, the release
# is broken — hold cutover, file a release.yml fix.

# 3. Render sentinel's children.inbox-acceptor.manifest_template from
#    sentinel/inbox-acceptor.template.toml in inbox/main HEAD. Substitute
#    the 7 placeholders inline (sentinel will substitute again at spawn
#    if the placeholders also appear in the live config).
```

### Sentinel children entry shape

```
children: {
  "inbox-acceptor": {
    manifest_template: <contents of sentinel/inbox-acceptor.template.toml>,
    default_package:   "https://github.com/colinrozzi/inbox/releases/download/<TAG>/inbox_acceptor-<STAGE_TAG>.wasm",
    secrets: {
      "BEARER_TOKEN":            "<contents of /etc/inbox/api-token>",
      "DKIM_PRIVATE_KEY":        "<PEM with newlines escaped as \\n>",
      "API_HANDLER_MANIFEST":    "https://github.com/colinrozzi/inbox/releases/download/<TAG>/inbox-api-handler-<STAGE_TAG>.toml",
      "MAILBOX_MANIFEST":        "https://github.com/colinrozzi/inbox/releases/download/<TAG>/inbox-mailbox-<STAGE_TAG>.toml",
      "ROUTER_MANIFEST":         "https://github.com/colinrozzi/inbox/releases/download/<TAG>/inbox-mailbox-router-<STAGE_TAG>.toml",
      "SMTP_ACCEPTOR_MANIFEST":  "https://github.com/colinrozzi/inbox/releases/download/<TAG>/inbox-smtp-acceptor-<STAGE_TAG>.toml",
      "SMTP_HANDLER_MANIFEST":   "https://github.com/colinrozzi/inbox/releases/download/<TAG>/inbox-smtp-handler-<STAGE_TAG>.toml"
    }
  }
}
```

`STAGE_TAG` is the `release-` prefix stripped: `release-20260531-d3b180d` → `20260531-d3b180d`.

`DKIM_PRIVATE_KEY` MUST be JSON-escaped (newlines as `\n`) because the value lands inside a JSON string field in the rendered initial_state. Raw newlines break JSON parse. One-shot escape: `awk '{printf "%s\\n", $0}' /etc/inbox/dkim/private.pem`.

### Cutover steps

```sh
# 0.5. Diff sentinel/inbox-acceptor.template.toml in inbox/main against
#      the prod /home/colin/work/actors/inbox/acceptor/manifest.toml.
#      If you find a stanza in prod that isn't in the in-repo template
#      (TLS blocks, store paths, handler sub-tables), STOP and update
#      the template first. The in-repo manifests are not authoritative
#      for prod — build_manifest.py historically injected prod-only
#      config at deploy time. Cutover attempt 2 on 2026-05-31 missed
#      [handler.server_tls] this way; 3-min outage.

# 1. Install rendered sentinel manifest (children map with inbox-acceptor
#    entry per above), restart sentinel. Phase 3.3 secret-validation will
#    refuse spawn if any placeholder lacks a matching secret.

# 2. systemctl stop inbox.service
#    Outage window starts.

# 3. nc sentinel {cmd:start, name:inbox-acceptor, package:<URL>}
#    Sentinel fetches the wasm, spawns inbox-acceptor. Acceptor parses
#    JSON initial_state, persists bearer + DKIM to store, spawns the
#    4 singleton sub-actors (mailbox-router, smtp-acceptor, etc.),
#    starts restoring mailboxes from /mnt/main-volume/inbox/store.

# 4. Poll until :443 AND :25 are bound AND TLS handshake completes.
#    Sequential mailbox restoration adds ~3s per mailbox to bind time;
#    a 10-mailbox store needs ~30s before the :443 listener is ready.
#    Don't gate cutover on a fixed sleep — use a poll loop:
until curl -sI --max-time 2 https://mail.colinrozzi.com:443 2>/dev/null | head -1 | grep -q HTTP; do sleep 2; done
until openssl s_client -connect mail.colinrozzi.com:25 -starttls smtp -quiet </dev/null 2>/dev/null | grep -q '220'; do sleep 2; done

# 5. Verify functionally: inbox CLI lookup of a known mailbox.
INBOX_TOKEN=$(cat /etc/inbox/api-token) inbox lookup colin@colinrozzi.com

# 6. systemctl disable inbox.service
#    Reboot-safety — prevents systemd from racing sentinel to bind :443/:25
#    on next host reboot.

# 7. KEEP /etc/systemd/system/inbox.service on disk for at least 1 week
#    as a rollback path. Delete only after sentinel-managed inbox is
#    stable through ≥1 sentinel restart and ≥1 fresh inbox release.
```

### Rollback

If the sentinel-managed inbox-acceptor fails health probes or shows unexpected behavior in the first hour:

```sh
nc sentinel {cmd:stop, name:inbox-acceptor}    # release :443 + :25
systemctl enable inbox.service
systemctl start inbox.service                  # systemd-managed inbox resumes
```

State on `/mnt/main-volume/inbox/store` is preserved across the rollback — mailboxes, router bindings, DKIM key, bearer token all carry over.

### Known gotchas

- **TLS bind timing**: mailbox restoration is sequential. ~3s per mailbox. A 10-mailbox store takes ~30s before `:443` accepts connections. A fixed-duration pre-cutover health gate (`sleep 8 && curl ...`) will fire before the listener is up. Use a poll loop (see step 4 above).
- **In-repo manifest drift**: in-repo `acceptor/manifest.toml` and the 5 sub-manifests lack TLS / cert-path blocks that the prod systemd `build_manifest.py` script injects. When updating `sentinel/inbox-acceptor.template.toml` or `.github/workflows/release.yml`'s inline sub-manifests, ALWAYS diff against the live VPS-deployed manifest first. The 3 manifests that need TLS config: inbox-acceptor (server_tls), api-handler (client_tls, auto_handshake=false), smtp-handler (server_tls).
- **TOML triple-quote nesting**: `sentinel/inbox-acceptor.template.toml`'s `initial_state` is a single-line literal `'...'` (not multi-line `'''...'''`). The sentinel outer manifest wraps the entire template body in a triple-quote literal; nested triple-quotes have no TOML escape and close the outer string early. Don't "improve readability" by re-introducing multi-line.
- **Phase 3.3 secret-validation regex**: matches the doubled-underscore placeholder pattern literally anywhere in the template body, including in comments. Don't write `__EXAMPLE_PLACEHOLDER__` as a literal in any doc text — use prose or angle-bracketed examples instead.
