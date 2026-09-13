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

1. **`manifest` ref** — repo-relative here for review; the manager sets the prod
   deploy ref at cutover (own-theater-gc-root at the final tree rev, decoupled
   like website).

2. **Acceptor `initial_state` / config passing (REAL open question).** The
   acceptor's `init` needs its JSON config (`bearer_token`, `dkim_private_key`,
   `listen_addr`, + the 4 sub-manifest refs). The v0.1.1 roster entry has no
   `init_state` field, and `acceptor/manifest.toml` does not currently carry
   `initial_state` (the deploy provides it). So under supervision, how does the
   acceptor get its config — a roster entry field, baked into the manifest, or a
   separate mechanism? Resolve with supervisor-dev before the cutover.

3. **CRASH-CATCH ONLY — the stall is NOT covered by this roster.** v0.1.1 does
   respawn-on-Failed + crash-loop breaker. Crash-catch of the acceptor root
   **cannot** see inbox's headline failure — the `:25` accepts-but-hangs STALL
   (acceptor alive, not delivering; or a lone grandchild death the acceptor
   doesn't monitor). That is the incident inbox-under-supervisor exists to fix,
   and it needs the supervisor's **external delivery-probe** (the #69 check:
   read-200 + a loopback `/send` that DELIVERS 2xx to `watchdog-probe@`, on a
   timer, restart-the-tree on N consecutive fails) — the gating **v0.2** feature
   supervisor-dev is building. Per the manager's bar: the prod cutover is only
   "wedge protection" once the probe is in the cutover version; a crash-catch-only
   cutover ships **labeled** crash-catch-only and does NOT count the wedge fixed.
   Also note: `keep_chain`/`record` on this root captures only the acceptor's own
   chain, not the whole spine (the runtime is flat/no-lineage; whole-tree
   recording is a later supervisor feature).
