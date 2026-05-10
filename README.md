# inbox

An agent-first email service built on [Theater](https://github.com/colinrozzi/theater). The interface is HTTPS — agents talk to their mailbox over a small JSON API, designed to fit how AI agents actually work (stateless, polling, cursor-based).

This is the seed: three actors, an HTTP/JSON API, no SMTP yet. Real email will land in this same mailbox once we add an SMTP gateway.

## API (current)

```
GET  /v1/inbox?since=<n>   → list messages with id ≥ n
POST /v1/messages          → store a message; returns {"id": ...}
                              body: {"from":"...", "to":"...", "subject":"...", "body":"..."}
POST /v1/send              → SMTP-deliver a message; also records it locally.
                              body: {"from":"...", "to":"...", "subject":"...", "body":"...",
                                     "smtp_server":"localhost:1025"}  // optional override
```

The cursor design assumes agents poll: agents remember the `next_cursor` from the last fetch and pass it as `since=` next time.

## SMTP

Inbound SMTP listens on `:1025` (smtp-acceptor + smtp-handler). External senders deliver mail via standard SMTP; messages land in the same mailbox the API serves.

Outbound SMTP is done synchronously from the api-handler via the `theater:simple/tcp` handler — it connects to whatever address the request specifies (default `localhost:1025`). Real-world deployments would use a relay like Postmark/SES instead of direct delivery.

### Known limitation: same-process self-loop

`POST /v1/send` to `localhost:1025` of the same theater instance currently deadlocks. The api-handler blocks on `tcp_receive` waiting for the SMTP greeting; meanwhile theater's `tcp_transfer` blocks the smtp-acceptor until the target's `handle-connection-transfer` returns; the target's first `tcp_send` is funneled through the same dispatch path. Two separate theater processes on different hosts (or even on different ports of the same host with two instances) work fine — this is the normal deployment shape.

## Architecture

```
acceptor                              (singleton, listens on :8080)
  │  on startup: spawns mailbox + smtp-acceptor
  │
  │  on each TCP connect:
  │    spawn api-handler, init with mailbox_id, transfer connection
  │
  ├── mailbox                         (singleton, long-lived state)
  │     list-since(cursor) -> page
  │     put-message(from, to, subject, body) -> id
  │
  ├── api-handler                     (one per HTTP connection, ephemeral)
  │     receives HTTP, routes, RPCs into mailbox, can SMTP-deliver outbound
  │
  ├── smtp-acceptor                   (singleton, listens on :1025)
  │     on each TCP connect:
  │       spawn smtp-handler, init with mailbox_id, transfer connection
  │
  └── smtp-handler                    (one per SMTP connection, ephemeral)
        SMTP server-side state machine; on DATA, RPCs put-message
```

Each connection-handling actor is single-shot — handles one connection then shuts down. Long-lived actors (acceptor, mailbox, smtp-acceptor) don't get tied up by misbehaving connections.

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
- [x] SMTP outbound (`POST /v1/send` connects to remote SMTP server)
- [x] SMTP inbound (`smtp-acceptor` + `smtp-handler` on `:1025`)
- [ ] Auth (Bearer tokens stored in mailbox)
- [ ] Multi-mailbox (one mailbox per address)
- [ ] Threads (group messages by `In-Reply-To` chain, expose `thread_id`)
- [ ] Async outbound delivery (relay actor instead of synchronous from api-handler) — also works around the same-process self-loop limitation
- [ ] Verified-sender flag (DKIM check on inbound)
- [ ] TLS termination at the TCP handler
- [ ] Deploy story

## License

Apache-2.0
