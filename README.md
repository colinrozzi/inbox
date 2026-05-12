# inbox

An agent-first email service built on [Theater](https://github.com/colinrozzi/theater). Agents talk to their mailbox over a small JSON HTTP API designed for how AI agents actually work (stateless, polling, cursor-based). Real internet email goes in and out via standard SMTP — DKIM-signed on the way out, MIME-parsed on the way in. Mailbox + router state persists to disk via the theater store handler. The HTTP API is bearer-token-authed so it's safe to talk to over the open internet, and there's a small theater-actor-based CLI that does exactly that. The reference deployment lives at `mail.colinrozzi.com`; see `RUNBOOK.md` for how to set up your own.

## API

```
POST /v1/mailboxes                          → register a new address (explicit)
                                              body: {"address":"agent@your.domain"}
GET  /v1/mailboxes                          → list registered addresses
GET  /v1/mailboxes/<addr>                   → look up an address (returns mailbox_id)

GET  /v1/mailboxes/<addr>/inbox?since=<n>   → list messages with id ≥ n
POST /v1/mailboxes/<addr>/messages          → direct insert (testing/admin)
                                              body: {"from","to","subject","body"}
POST /v1/mailboxes/<addr>/send              → SMTP-deliver from <addr>. No
                                              sender-copy is recorded; Bcc
                                              yourself if you want one (it
                                              travels the same SMTP path,
                                              looping back when the address
                                              is on this server's domain).
                                              body: {"to","subject","body",
                                                     "smtp_server":"..."  // optional}
```

The address in the URL path must be percent-encoded (`@` → `%40`).

Every route requires `Authorization: Bearer <token>`; missing or wrong returns `401`. The token is configured once on the deploy side (acceptor reads it from `initial_state` and writes it to the store under `api-bearer-token`); clients read it from `~/.config/inbox/token` or `$INBOX_TOKEN`.

The cursor design assumes agents poll: agents remember the `next_cursor` from the last fetch and pass it as `since=` next time.

## SMTP

**Inbound** on `:25`. The smtp-acceptor + smtp-handler implement an RFC 5321 server-side state machine: `EHLO`, `MAIL FROM`, `RCPT TO`, `DATA`. Unknown recipients are rejected with `550` at RCPT TO time (the handler asks the router whether the address is registered). On `DATA`, the message body is parsed for MIME — for `multipart/*` messages the first `text/plain` part is extracted; `quoted-printable` and `base64` transfer encodings are decoded. The clean text body is stored in each recipient's mailbox.

**Outbound** is done synchronously from the api-handler via the `theater:simple/tcp` handler. The api-handler builds the RFC 822 message, prepends a DKIM-Signature header (rsa-sha256, relaxed/relaxed canonicalization, selector `default`, domain configured per-deployment), and talks raw SMTP to whatever server the request specifies (default `localhost:25`). Production-grade deployments would point this at a relay (Postmark, SES, etc.) instead of speaking direct-to-MX, but direct-to-MX works fine once the operational basics are in place — see `RUNBOOK.md`.

## Architecture

```
acceptor                              (singleton, listens on :8080)
  │  reads <bearer-token>\n<DKIM PEM> from manifest initial_state and
  │  writes both to the shared store under `api-bearer-token` and `dkim-key`
  │  on startup: spawns mailbox-router + smtp-acceptor
  │
  │  on each TCP connect:
  │    spawn api-handler, init with router_id, transfer connection
  │
  ├── mailbox-router                  (singleton, holds address → mailbox map)
  │     register(address) -> mailbox_id   (spawns a fresh mailbox actor)
  │     lookup(address) -> option<mailbox_id>
  │     list() -> list<binding>
  │     persists bindings to `router-bindings`; eagerly re-spawns
  │     mailbox actors on restart from the saved list
  │
  ├── mailbox                         (one per registered address)
  │     list-since(cursor) -> page
  │     put-message(from, to, subject, body) -> id
  │     persists state to `mailbox:<address>` on every put
  │
  ├── api-handler                     (one per HTTP connection, ephemeral)
  │     loads the bearer token + DKIM key from the store at init;
  │     checks Authorization on every request, 401 on mismatch;
  │     /send signs outbound with DKIM before transmitting
  │
  ├── smtp-acceptor                   (singleton, listens on :25)
  │     on each TCP connect:
  │       spawn smtp-handler, init with router_id, transfer connection
  │
  └── smtp-handler                    (one per SMTP connection, ephemeral)
        SMTP server-side state machine; RCPT TO checks the router (rejects
        unknown recipients with 550); on DATA parses MIME and RPCs
        put-message on each recipient's mailbox.

cli                                   (one-shot, runs locally)
  │  reads a JSON command from manifest initial_state, tcp-connects to
  │  mail.<your domain>:8080 with Authorization: Bearer <token>, writes
  │  formatted output via theater:simple/terminal.write-stdout, shuts down
  └── built in the same workspace and shipped in the same nix output
```

Each connection-handling actor is single-shot — handles one connection then shuts down. Long-lived actors (acceptor, router, mailboxes, smtp-acceptor) don't get tied up by misbehaving connections.

## CLI

The `cli/` workspace member is a one-shot theater actor wrapped by a bash script. It builds in the same `nix build` as the server actors:

```sh
nix build .#default     # produces result/inbox_cli.wasm alongside the rest
```

Configure once:

```sh
export INBOX_API=mail.yourdomain.com:8080      # or whatever your deploy uses
mkdir -p ~/.config/inbox && cp /path/to/token ~/.config/inbox/token
```

Then:

```sh
./cli/inbox list
./cli/inbox new alice@yourdomain.com
./cli/inbox lookup alice@yourdomain.com
./cli/inbox read alice@yourdomain.com [--since N]
./cli/inbox send alice@yourdomain.com --to bob@example.com \
                  --subject "hi" --body "hello"
```

The wrapper generates a temp manifest with your args embedded in `initial_state` and runs `theater start` against the local theater binary at `result-theater/bin/theater`. Output goes to stdout via the `theater:simple/terminal` host functions.

## Running locally (full server)

```sh
# Build the wasms.
nix build .#default

# Generate a throwaway DKIM key:
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out /tmp/dkim.pem

# Pick a bearer token (any opaque string):
openssl rand -hex 32

# Put both in acceptor/manifest.toml's initial_state:
#   initial_state = """\
#   <bearer-token>
#   -----BEGIN PRIVATE KEY-----
#   ...
#   -----END PRIVATE KEY-----
#   """

theater start acceptor/manifest.toml

# In another shell — every request needs Authorization: Bearer <token>
TOKEN=<the token you generated>
curl -H "Authorization: Bearer $TOKEN" \
     -X POST -H 'Content-Type: application/json' \
     -d '{"address":"alice@example.com"}' \
     http://localhost:8080/v1/mailboxes
```

For a real deployment (real domain, real internet mail, systemd, GC roots), see `RUNBOOK.md`.

## Roadmap

- [x] HTTPS-style JSON API (acceptor + api-handler + mailbox)
- [x] SMTP outbound (`POST /v1/mailboxes/<addr>/send` connects to remote SMTP server)
- [x] SMTP inbound (`smtp-acceptor` + `smtp-handler` on `:25`)
- [x] Multi-mailbox via `mailbox-router` actor (one mailbox actor per address)
- [x] DKIM signing on outbound (`default._domainkey.<domain>`)
- [x] MIME body parsing on inbound (text/plain extraction; quoted-printable + base64)
- [x] Real-internet deployment (see `RUNBOOK.md`)
- [x] Mailbox + router persistence via `theater:simple/store` (survives restarts)
- [x] DKIM private key delivered via the shared store, not per-spawn init params
- [x] Cascade-resistant acceptors (a single failed connection doesn't kill the process)
- [x] systemd unit + nix GC roots (survives reboot, won't be garbage-collected)
- [x] Bearer-token auth on every HTTP route (single deployment-wide token for now)
- [x] Theater-actor-based CLI (`cli/inbox`) — local theater talks to the API over real-internet HTTP with the token
- [ ] Date + Message-ID headers on outbound (blocked on `theater:simple/timer.now()` from pack actors)
- [ ] api-handler pool (or single long-lived api-handler) — connections currently fail under burst load
- [ ] Users + per-user subdomains (e.g. `colin.agents.example.com`)
- [ ] Per-mailbox tokens (currently one shared token authorizes every route)
- [ ] STARTTLS on inbound + outbound (and HTTPS on the API — token over plain HTTP is fine for trial, not for production)
- [ ] Threads (group messages by `In-Reply-To` chain, expose `thread_id`)
- [ ] Async outbound delivery (relay actor instead of synchronous from api-handler)
- [ ] DKIM verification on inbound (currently only signs outbound; verified-sender flag)

## License

Apache-2.0
