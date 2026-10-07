# T0 — Inbox productization for the public Agent Inbox (v0, narrowed scope)

Status: DRAFT for review (inbox-dev, 2026-10-05). Greenlit by Colin via company-dev at reduced scope.
Audience: company-dev (control-plane), claude@ (sequencing), Colin.

This doc designs the **inbox-side (data-plane) T0 work** to run a **separate company instance** of
the inbox as a public multi-tenant mail service under the split-plane architecture:

- **Data plane = the inbox** (this repo). One mail interface; customers talk to it directly with a
  per-customer tenant token. Generic knobs live here.
- **Control plane = company** (separate repo/service). Thin, non-mail: signup, key lifecycle, tier/
  quota *config*, usage→billing, abuse policy, suspend. Owns what the knobs *mean*. Never a mail API.

It supersedes the first sizing pass with **two scope cuts Colin made on 2026-10-05**, both of which
delete the heaviest chunks:

- **Cut 1 — single shared domain, no BYO.** Everything is under **`agent-inbox.dev`**. ⇒ per-tenant
  DKIM **dropped** (one domain key is correct); per-tenant domain-ownership verification **deferred**.
- **Cut 2 — customers get `use`-only tokens; mailbox creation stays in the control-plane.** Customer
  keys carry the `use` cap only, **never `admin`**. Address allocation (`register-owned`) never leaves
  the control-plane. ⇒ the free-claim/impersonation risk is removed **by capability**, so no namespace-
  policy build is needed in the inbox.

---

## 0. Headline invariant (and its one caveat) — CONFIRMED IN CODE

> **Creating/claiming an address requires the `admin` capability; a `use`-only customer token cannot.
> The operator/`admin` capability never reaches a customer.**

Verified in `api-handler/src/lib.rs`:

- `authenticate()` (`:532-555`) is **fail-closed**. With `enforce=true`, every request's bearer must
  resolve via `registry.resolve` → `Caller::Tenant{id, caps}`; unknown/revoked/rpc-failure → **401**
  (`:539-542`, `registry_resolve :559-576`).
- `POST /v1/mailboxes` for a tenant (`handle_register_mailbox :671-674`) returns **403 unless the caller
  has the `admin` cap**, then calls `router.register-owned` which stamps ownership + enforces the
  reserved-list + one-tenant-per-mailbox (`mailbox-router/src/lib.rs:332-380`).
- Per-address read/write (`resolve_for_caller :607-620`) returns **404 (never 403)** when a tenant
  doesn't own the address — no existence leak. Uniform across every `/v1/mailboxes/<addr>/*` route
  (the single choke-point at `:489`, before the method match).
- Caps vocabulary is the closed set `{use, admin}` (`tenant-registry/src/lib.rs:47-48`). Minting a
  customer key with `caps=["use"]` is exactly the use-only token we want.

**THE ONE CAVEAT (must-fix for the company instance):** `Caller::Legacy::has_cap()` returns `true`
for **every** capability (`api-handler/src/lib.rs:523`). `Caller::Legacy` is produced only when
`enforce=false` (shared-bearer mode, `:543-553`), and the legacy `router.register` path it reaches is
owner-less and skips the reserved/valid checks. So the invariant holds **only when**:

1. the instance runs **`enforce=1`** (store flag `tenancy-enforce`, read at init), AND
2. **no usable shared bearer** is configured/distributed (ideally the `api-bearer-token` label is
   absent on the company instance), AND
3. customer keys are minted **`use`-only**.

**T0 ask:** on the company instance, make the Legacy path **unreachable** — simplest is a build/config
mode that hard-disables the `enforce=false` branch and the legacy `router.register` entirely, so there
is no "enforce off ⇒ all caps" foot-gun. (Small; see §2.)

---

## 1. The operator HTTP seam (control-plane ↔ inbox) — START HERE

Today there is **no operator HTTP API**. All provisioning is in-tree Theater RPC or a spawned,
spawn-gated `opsctl` WASM actor. The registry already implements the primitives we need — and three of
them are implemented but **unreachable** (zero callers): `mint-key`, `revoke-key`, `list-keys`
(`tenant-registry/src/lib.rs:130-135`). `create-tenant` and `router.register-owned` are reachable.

The seam the control-plane calls is a **new authed operator HTTP surface** on the inbox. Proposed:

| Method + route | Calls (existing export) | Notes |
|---|---|---|
| `POST /v1/admin/tenants` `{label, idempotency_key}` | `registry.create-tenant(label)` → `{tenant_id, root_token}` | **Make idempotent** (see below). Root token returned once. |
| `POST /v1/admin/tenants/<tid>/mailboxes` `{address}` | `router.register-owned(address, tid)` | Control-plane picks the local-part under `agent-inbox.dev`; inbox just allocates+stamps. 409 if taken/reserved. |
| `POST /v1/admin/tenants/<tid>/keys` `{caps:["use"], label}` | `registry.mint-key(tid, caps, label, created_by)` | Customer token minted here, **use-only**. Returned once. Wire the dead primitive. |
| `DELETE /v1/admin/keys/<kid>` | `registry.revoke-key(kid)` | Wire the dead primitive. Idempotent already. |
| `GET /v1/admin/tenants/<tid>/keys` | `registry.list-keys(tid)` | Wire the dead primitive (hashes/labels only, never clear tokens). |
| `POST /v1/admin/tenants/<tid>/limits` `{...}` | `registry.set-limits` (NEW, T1) | Deferred to T1 with quota enforcement. |

**Operator authentication.** The operator surface must be authed with a credential **distinct from
customer bearers**, and the check must sit at the `/v1/admin/*` prefix *before* any of the above.
Two options:

- **(A) Operator cap (recommended).** Extend the caps vocabulary with an `operator` capability
  (cross-tenant provisioning), mint a single operator tenant whose key carries it, and require
  `operator` on `/v1/admin/*`. Reuses the existing `registry.resolve` auth path, so the operator key
  gets rotation/revocation/list for free, and there's one auth mechanism not two. Cost: a small
  registry change (extend `caps_valid` + let create-tenant/mint-key issue `operator`).
- **(B) Dedicated operator bearer.** A separate secret (store label `operator-token`) checked at the
  prefix, mirroring the existing bearer/flag pattern. Simpler, but a second parallel auth mechanism
  and no built-in rotation.

Recommend **(A)**. Either way: **must be done before the operator API is exposed off-box.**

**Idempotency.** `create-tenant` is **not** idempotent today — a retried provision double-mints
(counter-driven). Add a client idempotency key: index label `tenant-idem-<key>` → `tenant_id`; on
repeat, return the existing tenant instead of minting. Matters because the control-plane will retry
over the network. (Small.)

**Why HTTP, not direct actor calls:** keeps the control-plane decoupled from rotating actor-ids
(router/registry ids rotate on restart and must be read from acceptor state), gives one authed,
idempotent, auditable seam, and keeps the control-plane out of the Theater tree.

---

## 2. Narrowed T0 feature list

1. **Confirm + lock the cap boundary** (§0). The check is correct and fail-closed *for Tenant callers*;
   the work is making the Legacy/`enforce=false` escape hatch unreachable on the company instance and
   adding a regression test: a `use`-only token gets 403 on `POST /v1/mailboxes` and 404 on any
   mailbox it doesn't own. **S.**
2. **Operator HTTP seam** (§1): routes + operator authn + wire the three dead primitives + idempotent
   create-tenant. **~1 week** (authn is the gating sub-task; the primitives already exist).
3. **/send egress allowlist (MX-only) — the SSRF/open-relay fix. BLOCKING.** Today `resolve_smtp_server`
   (`api-handler/src/lib.rs:887-894`) falls back to a **tenant-supplied `smtp_server`** from the request
   body and `tcp_connect`s to it (`:917`) — an untrusted tenant can aim outbound at any internal
   host:port or use us as an open relay. Fix: drop the body fallback; resolve recipients to real MX
   (or the known in-domain target) only; deny everything else. **S-M.**
4. **Single-domain DKIM/SPF/DMARC for `agent-inbox.dev`** — one DKIM key (the code already hardcodes a
   single `dkim::DOMAIN`/`SELECTOR`), plus correct SPF + DMARC DNS for the one domain. Per-tenant DKIM
   is explicitly **out** (Cut 1). Mostly deployment/DNS + confirming the signer. **S** (code) **+ DNS.**
5. **Hardening (shared-reputation domain ⇒ these matter more):**
   - **Inbound whole-message size cap.** `read_data_block` caps per *line* (10 MB) but enforces **no
     total-message cap** (`smtp-handler/src/lib.rs`), despite advertising `SIZE 10485760`. **S.**
   - **Mailbox O(n²) rewrite.** `mailbox` rewrites its **entire** state blob on every `put-message`
     (`mailbox/src/lib.rs:314 → save_state`). Same full-snapshot-per-event pathology chat just fixed in
     mesh (the 27s cold-sync stall; mesh `36affc5`). Move to append + periodic checkpoint, re-fold on
     boot. Also implicated in the current send-path stalls (separate thread). **M.**
   - **Hand-rolled MIME parser** on untrusted inbound (`parse_headers_and_body`, `extract_text_plain`,
     base64/qp decoders) — bound part-count/recursion/size; fuzz. **M.**
   - **Move spine secrets off the shared store namespace** for the company instance — DKIM key, token
     seed, bearer currently share the flat `inbox` store with tenant data; isolation is purely
     actor-layer. Separate store id / out-of-band for secrets. **M.**
6. **Shared-store dedup existence-leak guard (lower priority now).** Real but latent: content-refs are
   never serialized to clients and there is no by-ref/by-label fetch endpoint, and with use-only tokens
   there's no creation-probing surface. Keep refs off the API + a regression guard; revisit structurally
   only if/when raw/IMAP retrieval is built. **S.**
7. **Re-scoped adversarial security review for untrusted tenants — the hard gate, manager-run.** Smaller
   now (no namespace policy, no per-tenant DKIM, use-only customers), but still the go/no-go before
   onboarding untrusted token-holders. Headline invariant from §0 is the first thing to attack.

---

## 3. Re-sized effort

| Item | Effort | Was |
|---|---|---|
| 1. Cap-boundary lock + Legacy-path disable + regression test | S (~1d) | (new framing) |
| 2. Operator HTTP seam (routes + authn + wire primitives + idempotency) | ~1 wk | ~1 wk |
| 3. /send egress allowlist (SSRF/open-relay) | S-M (~1-2d) | S-M (blocking) |
| 4. Single-domain DKIM/SPF/DMARC | S + DNS | (was M-H per-tenant — **cut**) |
| 5a. Inbound message-size cap | S | S |
| 5b. Mailbox append/checkpoint (O(n²) fix) | M (~2-4d) | M |
| 5c. MIME parser hardening | M | M |
| 5d. Spine-secret store separation | M | M |
| 6. Dedup-leak guard | S | S (down-prioritized) |
| 7. Adversarial review (manager-run) | gate | gate |

**Deleted by the scope cuts:** per-tenant DKIM (M-H), domain-ownership verification (M-H), in-inbox
namespace policy (M). These were the long pole. **Net T0 ≈ 2–3 focused weeks + the review**, down from
the ~4–5 weeks in the first pass. Suggested order: **#1 + #3 first** (small, and #3 is blocking), then
**#2 (the seam)** in parallel with **#5b** (which also helps the live send-path degradation), then
**#4 + #5a/c/d**, then **#7** as the gate.

---

## 4. Explicitly OUT of scope for v0

- Per-tenant DKIM keys; bring-your-own-domain; DNS-TXT/assignment domain-ownership verification.
- In-inbox namespace/vanity policy (moves to the control-plane: it allocates local-parts under
  `agent-inbox.dev`, applies reserved words, and never issues an impersonating vanity).
- Structural cross-tenant dedup defeat (per-tenant store namespace / salted hashing).
- Quotas/metering beyond a basic abuse rate-limit — counts-first quota + pull metering stay T1/T2.
- Windowed rate-limits, bounce/complaint capture, self-service key management — T2 fast-follow.

---

## 5. What I need from the control-plane spec (open questions)

1. **Address allocation policy** (control-plane owns it): assigned/random local-part format under
   `agent-inbox.dev`? reserved-word list? Confirm the inbox only ever receives a fully-formed address
   to `register-owned` and applies no policy of its own beyond the 3 hardcoded reserved names.
2. **Operator authn choice:** OK with option (A) operator-cap? It needs the small caps-vocabulary
   extension; otherwise I'll do (B) a dedicated operator bearer.
3. **Idempotency key** shape on create-tenant (and whether register-mailbox/mint-key also need one).
4. **Suspend/kill-switch** mechanism: is suspend = `revoke-key` (all of a tenant's keys), or do you
   want a distinct tenant-level `suspended` flag the data plane checks at auth? (The latter is a small
   add and cleaner for reversible suspend.)
5. **Metering** granularity you'll actually bill on, so I scope the T1 pull exports to match.

---

## 6. Decisions (resolved 2026-10-05, company-dev id=765)

- **enforce=1 / no shared bearer / Legacy hard-disabled — ACCEPTED as unconditional.** The public
  instance runs `enforce=1` from day one, configures no usable shared bearer, and the
  `enforce=false`/legacy-`register` path is **compiled or configured out (or hard-refused)** when
  running as the public instance. This makes "create requires `admin`; operator creds never reach a
  customer" an *unconditional* invariant and closes the `Caller::Legacy::has_cap()==true` foot-gun.
  Baked into T0 item #1 + its regression test.
- **(a) Operator authn = the `operator` cap.** Caps vocabulary becomes `{use, admin, operator}`;
  `/v1/admin/*` is gated on `operator`. Control-plane holds exactly one (rotatable) operator key.
- **(b) Idempotency = an `Idempotency-Key` request header on all `/v1/admin/*` POSTs** (create-tenant,
  register-mailbox, mint-key); store key→result for a 24h window and replay on repeat. Additionally:
  create-tenant is idempotent on a caller-supplied external `customer_id` (same customer_id → same
  tenant, never a dup); mint-key must be idempotent (a retry must not mint two credentials);
  register-mailbox is idempotent on the address (re-register same address by same owner → no-op/200);
  revoke/DELETE is naturally idempotent.
- **(c) Suspend = a distinct tenant-level `suspended` flag checked at the auth choke-point** (NOT
  revoke-all). v0 semantics: suspended **blocks OUTBOUND send**; **inbound is still accepted + stored**
  (never drop a customer's mail); read/receive stays allowed. revoke-all-keys remains a separate,
  harder action for compromise/termination. → **New small T0 item (§2.8).**
- **(d) Address allocation is the control-plane's** — it hands the inbox a fully-formed
  `<localpart>@agent-inbox.dev` (v0 default: assigned random readable local-part, ~6-char base32,
  collision-checked by them). Inbox keeps its 3 hardcoded reserved names as a backstop and applies no
  other policy. Vanity/custom deferred.
- **(e) Metering dimensions (T1 pull exports): per-tenant MONTHLY `{sent, received, mailbox_count,
  bytes(optional)}`.** Primary bill/abuse signal = outbound `sent`; secondary = `mailbox_count`. No
  per-message event stream needed for v0.

### §2.8 Suspend flag (new T0 item, from decision (c))
A tenant-level `suspended: bool` on the `Tenant` record (forward-compatible field, no migration),
set/cleared via an operator route (`POST /v1/admin/tenants/<tid>/suspend` / `.../unsuspend`), surfaced
through `registry.resolve` (extend `Resolved`), and checked at the send choke-point: a suspended tenant
gets a 403 on `POST .../send` while inbound delivery and reads continue. **S-M.**

---

Appendix — key files: `api-handler/src/lib.rs` (auth/choke-point `:423-620`, register `:646-689`,
send `:804-1061`, egress resolve `:887-894`), `tenant-registry/src/lib.rs` (model `:52-93`, exports
`:130-135`, create/mint `:313-385`), `mailbox-router/src/lib.rs` (ownership + reserved `:107-380`),
`mailbox/src/lib.rs` (state rewrite `:314`), `opsctl/src/lib.rs` (current provisioning).

---

## 7. Sequencing + status (claude@ ruling 2026-10-05)

T0 is the primary track and proceeds uninterrupted. The separate "send-path degradation"
investigation (slow/duplicate `POST /send`) was root-caused in parallel; claude@'s ruling folds its
pieces around T0:

- **Mailbox incremental-persistence → FOLDED INTO T0 as a core spine item** (not a side-fix). A
  multi-tenant mail SERVICE cannot ship O(n²) full-snapshot-per-message writes (item §2.5b). Build it
  on the proven **mesh `36affc5`** pattern — persist chain-append + periodic checkpoint, re-fold on
  boot (validated live: 88K vs 2.1 GB, boot-from-chain-replay works). chat-dev/mesh-dev available to
  consult. This doubles as the fix for the live localhost send-path stalls.
- **Async accept-then-deliver** (POST /send enqueues + returns 202; async delivery worker with
  retry/backoff) → captured here as the **send-path architecture track within T0/product**. It's the
  proper fix for the synchronous-inline-SMTP hang and removes the client-retry→dupe dynamic entirely.
  Design pass pending; implement in this track.
- **Idempotency key on `POST /send`** → small **standalone PR**, opportunistic (a permanent dupe-killer
  independent of the "disable client auto-retry" interim). Low priority.
- **tcp read/connect timeouts** → **handed to theater-dev** (host ABI change: `tcp_receive`/`tcp_connect`
  take no deadline today). Not blocking.
- Interim holding the line now: container agents disable auto-retry on `POST /send` (claude@ broadcast).

### §2.3 egress decision (company-dev 2026-10-05): operator-configured SMARTHOST (option #1)
No in-guest MX resolution and no proxy — route all non-local outbound through a deploy-configured
smarthost relay; drop the client-supplied `smtp_server` fallback entirely (the SSRF/open-relay vector).
Deliverability lens (company-dev's call): an established sending provider gives warmed IPs, reputation
management, DKIM alignment, and the bounce/complaint feedback loops the abuse ladder needs — a self-run
MTA on a cold shared-domain IP is the wrong v0 risk. Provider + creds are deploy-time (Colin). In-guest
MX resolution / a theater DNS-MX host capability are documented as the future "self-send, no provider"
path — explicitly NOT v0.

### Implementation status
- **§2.1 cap-lock (public-instance, legacy-path hard-refused) — IMPLEMENTED: PR #105** (+
  `ops/public-instance-cap-lock-proof.sh`).
- **§2.3 smarthost relay + drop client `smtp_server` fallback + SMTP AUTH PLAIN (STARTTLS-gated) —
  IMPLEMENTED: PR #106.** Config via store labels `outbound-smarthost` / `smarthost-user` /
  `smarthost-pass` / `local-domain`; fleet instance unaffected (behavior changes only when a smarthost
  is configured). DKIM preserved.
- Both PRs are spine-adjacent → need a manager/Colin-gated deploy + the company-instance store labels
  (`public-instance=1`, `tenancy-enforce=1`, the smarthost config). I don't deploy.
- Next up: the operator seam (§1) in parallel with §2.5b mailbox incremental-persistence.

## 8. Operator-seam contract (company-dev, confirmed 2026-10-05) + 3-PR decomposition

Auth: all `/v1/admin/*` gated on the new `operator` cap (Bearer operator-key). The control-plane holds
exactly one operator key, **rotatable** (mint new → cut over → revoke old, via existing mint/revoke, so
no redeploy on suspected compromise). Spine secret; bootstrap out-of-band via opsctl at deploy (never
over the public API).

Routes: `POST /v1/admin/tenants` (create-tenant) · `POST /v1/admin/tenants/<tid>/mailboxes`
(register-owned; control-plane passes the fully-formed address) · `POST /v1/admin/tenants/<tid>/keys`
(mint, caps=`use` only) · `DELETE /v1/admin/keys/<kid>` (revoke) · `GET /v1/admin/tenants/<tid>/keys`
(list, hashes only) · `POST /v1/admin/tenants/<tid>/suspend` + `.../unsuspend` · (T1) set-limits.

Idempotency (load-bearing — the control-plane retries over a flaky transport): an `Idempotency-Key`
header on ALL admin POSTs → store key→{status, body} and **replay verbatim**, scoped per route+operator.
Implemented as **HTTP-response caching in the api-handler — no registry change**. Consequences:
- **mint-key token recovery:** the one-time plaintext token lives in the cached response body, so a
  replay re-returns the SAME token automatically (a dropped mint response can't strand a dead key).
  That cached body holds a plaintext token → store in the spine-secret namespace, **1h TTL** for mint
  (24h for other admin results).
- create-tenant is ALSO idempotent on a body `external_id` (the control-plane's opaque customer_id,
  first-write-wins → same tenant_id, never a dup). register-owned is idempotent on the address.

Address: control-plane hands `<localpart>@agent-inbox.dev` fully formed (v0 = assigned random ~6-char
base32, collision-checked + reserved-list by them). The inbox applies no namespace policy beyond its 3
hardcoded reserved names.

Suspend: reversible flag checked at auth; blocks OUTBOUND, keeps accepting INBOUND, allows read.
revoke-all stays a separate hard action.

### 3-PR decomposition + status
- **PR A — tenant-registry foundation: IMPLEMENTED, PR #107.** `operator` cap in the vocabulary +
  reversible `suspended` flag on Tenant (`set-suspended` export + surfaced in `resolve`). Registry-only,
  independently deployable (api-handler ignores the extra `resolve` field until PR B).
- **PR B — api-handler operator routes** (next): `/v1/admin/*` gated on `operator`; wire create-tenant /
  mint (use-only) / revoke / list / register-owned / suspend+unsuspend; + the suspended-blocks-outbound
  check at the send path.
- **PR C — api-handler idempotency** (after B): `Idempotency-Key` response-cache (1h mint / 24h others,
  per route+operator) + create-tenant `external_id` natural key + register-owned address idempotency.
