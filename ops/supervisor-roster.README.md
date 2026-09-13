# inbox supervisor roster (`supervisor-roster.json`)

The reviewable artifact for the Path-B **inbox-under-supervisor** cutover (the
manager sequences the prod re-parent; this is not deployed standalone). Consumed
by `supervisor spawn ops/supervisor-roster.json --control-port` (colinrozzi/supervisor).

**Validated** against the real supervisor v0.1.1 binary: the roster parses and
the supervisor generates its manifest + drives to the actor spawn (fails only on
the acceptor's dev wasm path being absent in a bare container — environmental,
not a schema issue).

## Shape

**One entry — the acceptor (the spine's tree root).** The acceptor spawns
mailbox-router + smtp-acceptor at init (and api-handler per HTTP connection) via
`runtime.spawn`; smtp-acceptor spawns smtp-handler per SMTP connection; the
router lazy-spawns mailboxes. So supervising the acceptor root brings up the
whole inbox spine = the converged "whole-inbox-tree as one supervised unit."

Entry fields (supervisor v0.1.1): `handle`, `manifest` (path or git/http ref),
`max` + `window_ms` (crash-loop breaker), `keep_chain` (in-memory last-200
events, queryable via `supervisor chain inbox`). For a persisted black box use
`record: {"kind":"file","path":"inbox.chain.jsonl"}` + run with `--record-dir`
instead of `keep_chain`.

## Open items for the cutover (supervisor-dev / manager — do NOT treat final)

1. **`manifest` ref** — repo-relative (config-less) here for review; at cutover
   the manager points it at the **deploy-generated acceptor manifest that carries
   `initial_state`** (see 2), own-theater-gc-root at the final tree rev, decoupled
   like website.

2. **Acceptor `initial_state` / config passing — RESOLVED: manifest-carries-config,
   no supervisor change.** The supervisor spawns each entry via
   `runtime.spawn(manifest, init_state=None)`; `None` → the host falls back to the
   child manifest's `initial_state` (identical to how `theater spawn` passes config
   today — README: the acceptor reads bearer/DKIM + sub-manifest refs from manifest
   `initial_state`). So the supervised acceptor gets its config from the acceptor
   manifest's `initial_state`, exactly as in the current deploy. SECRETS: the prod
   `acceptor.toml` is **deploy-generated on the box** (template + injected
   bearer/DKIM, gitignored) — the committed manifest here is the config-less
   structural reference. (We DECLINED supervisor-dev's optional roster `init_state`
   override: inbox doesn't need it, it'd force secrets/placeholders into the
   committed roster, and manifest-carries-config keeps the supervisor generic +
   matches today's model at lowest cutover risk.)

3. **CRASH-CATCH ONLY — the `:25` stall is NOT covered by the supervisor (Colin's
   ruling).** The supervisor stays generic crash-catch: respawn-on-Failed +
   crash-loop breaker + the flight-recorder chain. Crash-catch of the acceptor
   root **cannot** see inbox's headline failure — the `:25` accepts-but-hangs
   STALL (acceptor alive, not delivering; or a lone grandchild death the acceptor
   doesn't monitor). Colin ruled the delivery-check (`healthy` = read-200 + a
   loopback `/send` that DELIVERS 2xx) is **application-level logic, deferred** —
   not the generic supervisor's job. So this cutover ships **labeled** "crash-catch
   + flight-recorder," NOT wedge-fixed.

   **THEREFORE the standalone systemd watchdog (`ops/inbox-watchdog.sh`, the #69
   delivery-check) STAYS — it is the only stall-catch.** The earlier plan to
   retire it once a supervisor probe proved out is VOID (the probe is deferred).
   The cutover MUST keep the watchdog functioning after inbox is re-parented under
   the supervisor (verify its restart path still applies once `supervisor spawn`
   is inbox's host) so wedge protection is not silently dropped. Future delivery-
   probe = application-level (an inbox-side health actor, or a supervisor-driven
   probe config down the line; the #69 watchdog logic seeds it) — not scoped now.

   Also: `keep_chain`/`record` on this root captures only the acceptor's own
   chain, not the whole spine (the runtime is flat/no-lineage; whole-tree
   recording is a later supervisor feature).
