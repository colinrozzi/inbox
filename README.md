# inbox

An agent-first email service built on [Theater](https://github.com/colinrozzi/theater). Agents talk to their mailbox over a small JSON HTTP API designed for how AI agents actually work (stateless, polling, cursor-based). Real internet email goes in and out via standard SMTP — DKIM-signed on the way out, MIME-parsed on the way in. Mailbox + router state persists to disk via the theater store handler. The reference deployment lives at `mail.colinrozzi.com`; see `RUNBOOK.md` for how to set up your own.

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

The cursor design assumes agents poll: agents remember the `next_cursor` from the last fetch and pass it as `since=` next time.

## SMTP

**Inbound** on `:25`. The smtp-acceptor + smtp-handler implement an RFC 5321 server-side state machine: `EHLO`, `MAIL FROM`, `RCPT TO`, `DATA`. Unknown recipients are rejected with `550` at RCPT TO time (the handler asks the router whether the address is registered). On `DATA`, the message body is parsed for MIME — for `multipart/*` messages the first `text/plain` part is extracted; `quoted-printable` and `base64` transfer encodings are decoded. The clean text body is stored in each recipient's mailbox.

**Outbound** is done synchronously from the api-handler via the `theater:simple/tcp` handler. The api-handler builds the RFC 822 message, prepends a DKIM-Signature header (rsa-sha256, relaxed/relaxed canonicalization, selector `default`, domain configured per-deployment), and talks raw SMTP to whatever server the request specifies (default `localhost:25`). Production-grade deployments would point this at a relay (Postmark, SES, etc.) instead of speaking direct-to-MX, but direct-to-MX works fine once the operational basics are in place — see `RUNBOOK.md`.

## Architecture

```
acceptor                              (singleton, listens on :8080)
  │  reads its DKIM private key from manifest initial_state
  │  on startup: spawns mailbox-router + smtp-acceptor
  │
  │  on each TCP connect:
  │    spawn api-handler, init with (router_id, dkim_private_key_pem),
  │    transfer connection
  │
  ├── mailbox-router                  (singleton, holds address → mailbox map)
  │     register(address) -> mailbox_id   (spawns a fresh mailbox actor)
  │     lookup(address) -> option<mailbox_id>
  │     list() -> list<binding>
  │
  ├── mailbox                         (one per registered address)
  │     list-since(cursor) -> page
  │     put-message(from, to, subject, body) -> id
  │
  ├── api-handler                     (one per HTTP connection, ephemeral)
  │     receives HTTP, routes, RPCs the right mailbox.
  │     /send signs outbound with DKIM before transmitting.
  │
  ├── smtp-acceptor                   (singleton, listens on :25)
  │     on each TCP connect:
  │       spawn smtp-handler, init with router_id, transfer connection
  │
  └── smtp-handler                    (one per SMTP connection, ephemeral)
        SMTP server-side state machine; RCPT TO checks the router (rejects
        unknown recipients with 550); on DATA parses MIME and RPCs
        put-message on each recipient's mailbox.
```

Each connection-handling actor is single-shot — handles one connection then shuts down. Long-lived actors (acceptor, router, mailboxes, smtp-acceptor) don't get tied up by misbehaving connections.

## Running locally

```sh
# Build the wasms (or `nix build`).
cargo build --release --target wasm32-unknown-unknown

# Generate a throwaway DKIM key for local testing:
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out /tmp/dkim.pem
# Then put the PEM contents in acceptor/manifest.toml's initial_state:
#   initial_state = """\
#   -----BEGIN PRIVATE KEY-----
#   ...
#   -----END PRIVATE KEY-----
#   """

theater start acceptor/manifest.toml

# In another shell:
curl -X POST -H 'Content-Type: application/json' \
  -d '{"address":"alice@example.com"}' \
  http://localhost:8080/v1/mailboxes

curl 'http://localhost:8080/v1/mailboxes/alice%40example.com/inbox?since=0'

curl -X POST -H 'Content-Type: application/json' \
  -d '{"to":"alice@example.com","subject":"hi","body":"loop test","smtp_server":"localhost:25"}' \
  http://localhost:8080/v1/mailboxes/alice%40example.com/send
```

For a real deployment (real domain, real internet mail), see `RUNBOOK.md`.

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
- [ ] Date + Message-ID headers on outbound (blocked on `theater:simple/timer.now()` from pack actors)
- [ ] api-handler pool (or single long-lived api-handler) — connections currently fail under burst load
- [ ] Users + per-user subdomains (e.g. `colin.agents.example.com`)
- [ ] Auth (Bearer tokens scoped per mailbox / per user)
- [ ] Threads (group messages by `In-Reply-To` chain, expose `thread_id`)
- [ ] Async outbound delivery (relay actor instead of synchronous from api-handler)
- [ ] DKIM verification on inbound (currently only signs outbound; verified-sender flag)
- [ ] STARTTLS support (inbound + outbound)

## License

Apache-2.0
