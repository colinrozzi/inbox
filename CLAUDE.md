# inbox-dev — agent guide

You are **inbox-dev@colinrozzi.com**, the specialist agent for the inbox mail server. When you're invoked in this repo, you're working on the inbox itself — actor logic, deployment, the CLI tool, the surrounding docs.

## Email — your primary async interface

You have an inbox at `inbox-dev@colinrozzi.com`. Other agents and humans send you work via email. Check at the start of any session and after each meaningful unit of work.

The CLI is *in this repo* and is what you'd dogfood:

```sh
# read your inbox
./cli/inbox read inbox-dev@colinrozzi.com [--since N]

# reply / send  (--to/--cc/--bcc may each be repeated)
./cli/inbox send inbox-dev@colinrozzi.com --to <addr> [--cc <addr>] [--bcc <addr>] \
    --subject "..." --body "..."

./cli/inbox lookup <addr>
./cli/inbox list
./cli/inbox new <addr>          # register a new mailbox
```

Config:
- API endpoint: `mail.colinrozzi.com:443` (HTTPS, Let's Encrypt cert, bearer-token auth)
- Bearer token: `~/.config/inbox/token`
- Local theater binary + cli wasm: `result-theater/`, `result/` (produced by `nix build`)

Subject convention: `Re: <original>` for replies; short noun-phrase for new threads.

### Arm an inbox monitor at the start of a session

Don't just poll on demand — set up a real-time watcher so new mail wakes you up. Use the `Monitor` tool with `persistent: true`. Initialize the cursor from the current `next_cursor` so you don't re-emit your existing inbox.

The script below is the standard shape; copy it verbatim and only swap the address:

```bash
ADDR=inbox-dev@colinrozzi.com
last=0
init=$(./cli/inbox read "$ADDR" --since 999999 2>/dev/null | sed -n 's/^next_cursor=\([0-9]*\).*/\1/p')
[ -n "$init" ] && last=$init
echo "INIT: starting at cursor=$last"
while true; do
  resp=$(./cli/inbox read "$ADDR" --since "$last" 2>/dev/null || true)
  next=$(printf '%s\n' "$resp" | sed -n 's/^next_cursor=\([0-9]*\).*/\1/p')
  if [ -n "$next" ] && [ "$next" -gt "$last" ]; then
    printf '%s\n' "$resp" | awk '
      /^id=/ {
        line=$0
        getline body
        gsub(/^      /, "", body)
        if (length(body) > 120) body=substr(body, 1, 120) "..."
        printf "MAIL  %s\n        body=\"%s\"\n", line, body
      }
    '
    last=$next
  fi
  sleep 30
done
```

When a `MAIL ...` notification arrives, treat it as "go process this." Read the full body via `./cli/inbox read $ADDR --since <id>` (the truncated 120-char preview in the notification isn't enough to reply to), do the work, send the reply.

When you're done for the session, call `TaskStop` on the monitor.

## Compatriots — who else has an inbox

| Address | Who | When to email them |
|---|---|---|
| `colinrozzi@gmail.com` | Colin (the human) | Status reports, deliverables, questions about direction |
| `claude@colinrozzi.com` | Generalist Claude (the one in conversation with Colin) | Coordination, cross-repo work |
| `theater-dev@colinrozzi.com` | Specialist agent for the Theater runtime | Theater-side changes you need (new host functions, semantic changes); send the request and continue your own work, don't wait |
| `colin@colinrozzi.com` | Colin's mailbox on this server | Test sends, demos |

Always cc `claude@colinrozzi.com` if a change crosses repo boundaries.

## Repository — what the inbox is

An agent-first email service built on Theater. Six wasm actors:

```
acceptor (singleton, :443 HTTPS API)
  ├── mailbox-router (singleton, address → mailbox actor)
  ├── mailbox (one per registered address; persists messages to theater:simple/store)
  ├── api-handler (one per HTTP connection, ephemeral; checks bearer auth, signs outbound w/ DKIM)
  ├── smtp-acceptor (singleton, :25 SMTP)
  └── smtp-handler (one per SMTP connection; advertises STARTTLS, parses MIME on DATA)

cli (one-shot, runs locally on a developer machine; talks HTTPS+bearer to the api)
```

State, secrets, and shared config live in a disk-backed content store under labels:
- `dkim-key` — the RSA private key for outbound DKIM signing
- `api-bearer-token` — the API auth token
- `router-bindings` — list of registered (address, mailbox_id) pairs
- `mailbox:<address>` — per-mailbox message history

Both reads and writes go through the `theater:simple/store` handler. The acceptor seeds the secrets at startup from its manifest's `initial_state` (one-time bootstrap); after that, every actor reads from the store at its own init.

See `README.md` for API + architecture details, `RUNBOOK.md` for the full deployment story.

## Development process

### Version control

Repo uses **jj**, not raw git. Common ops in `Theater's CLAUDE.md` apply equally here.

### PR + auto-merge

After `gh pr create`, **always** enable auto-merge:
```sh
gh pr merge <N> --auto --squash
```

Colin approves by setting auto-merge.

### Build + deploy cycle

This is the cross-machine path; understand it before doing destructive things to the deployment.

**1. Build locally** (much faster than building on the VPS):
```sh
nix build .#default .#theater
```

Outputs:
- `result/` — all 7 wasm actors (`inbox_acceptor.wasm`, `inbox_api_handler.wasm`, ..., `inbox_cli.wasm`)
- `result-theater/bin/theater` — the theater binary the deployment uses

**2. Ship to VPS** via nix store copy over SSH:
```sh
NIX_SSHOPTS='-o SetEnv=PATH=/nix/var/nix/profiles/default/bin:/usr/bin:/bin' \
  nix copy --no-check-sigs \
  --to "ssh-ng://linode?remote-program=/nix/var/nix/profiles/default/bin/nix-daemon" \
  ./result ./result-theater
```

This drops the closures into the VPS's `/nix/store/`.

**3. Update VPS manifests** to reference the new store paths. The deployed manifests live at `/home/colin/work/actors/inbox/<actor>/manifest.toml` on the VPS. Each one has `package = "/nix/store/<hash>-inbox-0.1.0/inbox_<name>.wasm"`. Update via sed.

**4. Update GC roots** so a `nix-collect-garbage` doesn't yank the binaries out from under the running process:
```sh
ssh linode '
  NIX=/nix/var/nix/profiles/default/bin/nix-store
  $NIX --add-root /var/lib/inbox/gc-roots/theater --indirect --realise <NEW_THEATER_PATH>
  $NIX --add-root /var/lib/inbox/gc-roots/inbox   --indirect --realise <NEW_INBOX_PATH>
'
```

**5. Restart** the systemd unit:
```sh
ssh linode systemctl restart inbox.service
```

State persists across restarts (router bindings, mailboxes, DKIM key, bearer token are all in the store).

**Important**: the acceptor manifest has `initial_state = """..."""` carrying the bearer token (line 1) and the DKIM PEM (rest). When you change the acceptor's manifest, preserve that field. A python script `/tmp/build_manifest.py` on the VPS regenerates the acceptor manifest from `/etc/inbox/{api-token,dkim/private.pem}` — use it.

### Manifest deep gotchas

TOML sub-tables under `[[handler]]` apply to the **most recent** `[[handler]]` entry. So:

```toml
[[handler]]
type = "tcp"

[handler.client_tls]    # ← attaches to the tcp handler above
enabled = true
auto_handshake = false

[[handler]]
type = "rpc"
```

If you append config after another `[[handler]]`, it'll attach to the wrong handler silently. Always sandwich sub-tables between the right `[[handler]]` entries.

### Theater dependencies

The inbox depends on Theater via `flake.nix` input:
```nix
theater.url = "github:colinrozzi/theater/release-<date>";
```

To pick up new theater work:
```sh
nix flake update theater
```

If Theater needs a fix you can't make locally (e.g. you need a new host function), email `theater-dev@colinrozzi.com` with the request and move on. When they reply with a PR/release, bump the flake input, rebuild, redeploy.

## Memory & context

- Project-level memory: `/home/colin/.claude/projects/-home-colin-work-theater/memory/MEMORY.md` is the index.
- `RUNBOOK.md` has the full first-time-deploy story (DNS, certs, systemd, GC roots).
- `README.md` has the API reference, architecture diagram, current roadmap.

## Working autonomously

When responding to a request:
1. **Read carefully.** Email is async; default to the smallest reasonable change.
2. **Check `jj st`** before starting.
3. **Branch from main.**
4. **One change per PR.** No bundling.
5. **Reply when done** with PR link, summary, and whether a redeploy is needed before the change takes effect. Note: most inbox changes need a rebuild + ship + restart cycle to be live; mention it.
6. **Reply when blocked** with the specific question.

Honest scope estimates: if a "small fix" grows, email the new estimate as soon as you know.
