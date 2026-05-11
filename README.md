# inbox

An agent-first email service built on [Theater](https://github.com/colinrozzi/theater). The interface is HTTPS — agents talk to their mailbox over a small JSON API, designed to fit how AI agents actually work (stateless, polling, cursor-based).

This is the seed: three actors, an HTTP/JSON API, no SMTP yet. Real email will land in this same mailbox once we add an SMTP gateway.

## API (current)

```
POST /v1/mailboxes                          → register a new address (explicit)
                                              body: {"address":"agent@your.domain"}
GET  /v1/mailboxes                          → list registered addresses

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

Inbound SMTP listens on `:25` (smtp-acceptor + smtp-handler). External senders deliver mail via standard SMTP; messages land in the same mailbox the API serves.

Outbound SMTP is done synchronously from the api-handler via the `theater:simple/tcp` handler — it connects to whatever address the request specifies (default `localhost:25`). Real-world deployments would use a relay like Postmark/SES instead of direct delivery.


## Architecture

```
acceptor                              (singleton, listens on :8080)
  │  on startup: spawns mailbox-router + smtp-acceptor
  │
  │  on each TCP connect:
  │    spawn api-handler, init with router_id, transfer connection
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
  │     receives HTTP, looks up mailbox via router, RPCs the right mailbox
  │
  ├── smtp-acceptor                   (singleton, listens on :1025)
  │     on each TCP connect:
  │       spawn smtp-handler, init with router_id, transfer connection
  │
  └── smtp-handler                    (one per SMTP connection, ephemeral)
        SMTP server-side state machine; RCPT TO checks the router (rejects
        unknown recipients with 550); on DATA, RPCs put-message on each
        recipient's mailbox.
```

Each connection-handling actor is single-shot — handles one connection then shuts down. Long-lived actors (acceptor, router, mailboxes, smtp-acceptor) don't get tied up by misbehaving connections.

## Running

```sh
cargo build --release --target wasm32-unknown-unknown
theater start acceptor/manifest.toml

# in another shell:
curl http://localhost:8080/v1/inbox
curl -X POST -H 'Content-Type: application/json' \
  -d '{"from":"alice","to":"bob","subject":"hi","body":"hello"}' \
  http://localhost:8080/v1/messages
curl 'http://localhost:8080/v1/inbox?since=1'
```

## Roadmap

- [x] HTTPS-style JSON API (acceptor + api-handler + mailbox)
- [x] SMTP outbound (`POST /v1/mailboxes/<addr>/send` connects to remote SMTP server)
- [x] SMTP inbound (`smtp-acceptor` + `smtp-handler` on `:1025`)
- [x] Multi-mailbox via `mailbox-router` actor (one mailbox actor per address)
- [ ] Users + per-user subdomains (e.g. `colin.agents.example.com`)
- [ ] Auth (Bearer tokens scoped per mailbox / per user)
- [ ] Threads (group messages by `In-Reply-To` chain, expose `thread_id`)
- [ ] Async outbound delivery (relay actor instead of synchronous from api-handler)
- [ ] Verified-sender flag (DKIM check on inbound)
- [ ] TLS termination at the TCP handler
- [ ] Deploy story

## License

Apache-2.0
