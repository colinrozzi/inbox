# inbox

An agent-first email service built on [Theater](https://github.com/colinrozzi/theater). The interface is HTTPS — agents talk to their mailbox over a small JSON API, designed to fit how AI agents actually work (stateless, polling, cursor-based).

This is the seed: three actors, an HTTP/JSON API, no SMTP yet. Real email will land in this same mailbox once we add an SMTP gateway.

## API (current)

```
GET  /v1/inbox?since=<n>   → list messages with id ≥ n
POST /v1/messages          → store a message; returns {"id": ...}
                              body: {"from":"...", "to":"...", "subject":"...", "body":"..."}
```

The cursor design assumes agents poll: agents remember the `next_cursor` from the last fetch and pass it as `since=` next time.

## Architecture

```
acceptor                              (singleton)
  │  listens on :8080
  │  spawned mailbox once at startup
  │
  │  on each TCP connect:
  │    spawn api-handler, init it with mailbox_id, transfer connection
  │
  ├── mailbox                         (singleton, long-lived state)
  │     list-since(cursor)
  │     put-message(from, to, subject, body)
  │
  └── api-handler                     (one per connection, ephemeral)
        receives HTTP, routes, RPCs into mailbox, sends JSON, shuts down
```

The api-handler being one actor per connection means each request runs in
isolation — a bad request can't affect the listener or the mailbox state.

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

- [ ] Auth (Bearer tokens stored in mailbox)
- [ ] Multi-mailbox (one mailbox per address)
- [ ] Threads (group messages by `In-Reply-To` chain, expose `thread_id`)
- [ ] SMTP outbound via Postmark/SES
- [ ] SMTP inbound via webhook from same provider
- [ ] Verified-sender flag (DKIM check on inbound)
- [ ] TLS termination at the TCP handler
- [ ] Deploy story

## License

Apache-2.0
