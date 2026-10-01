//! inbox-opsctl: one-shot operator-setup actor.
//!
//! The manager's theater CLI can `spawn` but has no store verb and no rpc verb,
//! so it can't seed store labels or trigger registry RPCs. This actor is the
//! missing mechanism: SPAWN it with a JSON config and on init it performs the
//! out-of-band operator setup — writes store labels (seed secrets, flip flags)
//! and drives registry/router RPCs (create-tenant, import-key, register-owned) —
//! then logs the results (incl. create-tenant root tokens) to its chain.
//!
//! One-shot: it does everything in `init`, then idles. Privileged (writes
//! secrets, mints tenants) — spawning it IS the operator action, gated by whoever
//! controls theater spawn. Handlers: self (log), store (write), rpc (call).
//!
//! Covers both the two-tenant proof setup (labels + create-tenant x2) AND the
//! prod cutover (seed tenant-registry-seed, create fleet, import the shared token,
//! register-owned every existing address, flip tenancy-enforce).

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};
use serde::Deserialize;
use theater_guest::State;

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";

#[derive(Clone, GraphValue, State)]
pub struct OpsState {
    pub done: bool,
}

pack_types! {
    imports {
        theater:simple/self {
            log: func(msg: string),
        }
        theater:simple/store {
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
        theater:simple/rpc {
            call: func(actor-id: string, function: string, params: value, options: value) -> value,
        }
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:simple/actor.get-state: func() -> value,
    }
}

#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

#[import(module = "theater:simple/rpc", name = "call")]
fn rpc_call(actor_id: String, function: String, params: Value, options: Value) -> Value;

fn ok_unit() -> Value {
    let u = Value::Tuple(vec![]);
    Value::Result { ok_type: u.infer_type(), err_type: ValueType::String, value: Ok(Box::new(u)) }
}
/// One-shot semantics: opsctl must NOT fail its init, or the supervisor respawns
/// it (rate-limited 5/60s, then trips the breaker) — supervisor-dev's nuance. So
/// on any problem we LOG it prominently to the chain and return a CLEAN Ok: the
/// operator reads the outcome via `supervisor chain <handle>`, and a failed setup
/// halts here without a respawn loop. (Success + failure both leave a readable
/// chain; neither respawns.)
fn abort(msg: &str) -> Value {
    log(format!("[opsctl] ABORT — halted, NOT respawned; read this chain: {}", msg));
    OpsState::set(OpsState { done: false });
    ok_unit()
}

/// Best-effort stringify of a Value for error/diagnostic messages. Intentionally
/// Debug-free (packr Value's Debug impl isn't relied on): err payloads are strings
/// in practice, and the non-string arms only feed rare "unexpected shape" messages.
fn stringify(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Record { .. } => String::from("<record>"),
        Value::Variant { case_name, .. } => format!("<variant:{}>", case_name),
        Value::Option { .. } => String::from("<option>"),
        Value::Tuple(_) => String::from("<tuple>"),
        _ => String::from("<non-string value>"),
    }
}

/// Abort message for an RPC that returned an unexpected (non-error) ok shape.
fn bad_shape(op: &str, v: Value) -> String {
    format!("opsctl: {} unexpected ok shape: {}", op, stringify(v))
}

/// Peel the result-layers off an RPC return, returning Ok(payload) | Err(reason).
///
/// theater's `rpc.call` transport-wraps the callee's return, and on prod
/// (theater 0.3.9, pre-#216) a `result<T,E>` is delivered NOT as a first-class
/// Value::Result but as a TAGGED VARIANT: `Variant{tag:0, payload:[T]}` for Ok,
/// `Variant{tag:1, payload:[E]}` for Err (some ABI vintages set case_name "ok"/"err"
/// instead of / in addition to the numeric tag). The callee's OWN result is nested
/// inside, so there can be TWO result layers (outer transport variant + inner native
/// result) — we peel ITERATIVELY until the first value that is NOT a result wrapper
/// (the callee's actual payload: Record for create-tenant, String for import-key /
/// register-owned). The old code matched ONLY Value::Result, so on 0.3.9 the outer
/// Variant fell through and opsctl aborted AFTER the registry had already mutated —
/// the B1 blocker. This accepts BOTH the tagged-variant shape AND the post-#216
/// native Value::Result, so it is forward-compatible. An Err at ANY layer returns
/// Err(reason), preserving the ok/err discriminant (critical: result<string,string>
/// ok and err are both bare strings, indistinguishable once unwrapped). We do NOT
/// require type_name=="result" on the variant — opsctl's payloads are never
/// themselves variants, so recognising any tag-0/1 (or ok/err) variant as a result
/// wrapper is safe and also covers the bare-tag vintage. Mirrors the proven
/// api-handler peel + supervisor-dev's runtime_spawn peel.
fn unwrap_result(v: Value) -> Result<Value, String> {
    let mut v = v;
    loop {
        v = match v {
            Value::Result { value: Ok(inner), .. } => *inner,
            Value::Result { value: Err(e), .. } => return Err(stringify(*e)),
            Value::Variant { tag, case_name, payload, .. }
                if tag == 1 || case_name.eq_ignore_ascii_case("err") =>
            {
                return Err(payload
                    .into_iter()
                    .next()
                    .map(stringify)
                    .unwrap_or_else(|| String::from("rpc variant err (no payload)")));
            }
            Value::Variant { tag, case_name, mut payload, .. }
                if (tag == 0 || case_name.eq_ignore_ascii_case("ok")) && payload.len() == 1 =>
            {
                payload.remove(0)
            }
            // Not a result wrapper -> the callee's actual payload.
            other => return Ok(other),
        };
    }
}
/// Read a string field from a Record, tolerant of snake vs kebab keys.
fn record_field(fields: &[(String, Value)], key: &str) -> String {
    let kebab = key.replace('_', "-");
    fields
        .iter()
        .find(|(k, _)| k == key || k == &kebab)
        .and_then(|(_, v)| if let Value::String(s) = v { Some(s.clone()) } else { None })
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct Cfg {
    /// Store labels to write (Gap 1: seed secrets / flip flags). {name, value}.
    #[serde(default)]
    labels: Vec<Lbl>,
    #[serde(default)]
    registry_id: String,
    #[serde(default)]
    router_id: String,
    /// Human labels; create-tenant each -> logs {tenant_id, root_token} (Gap 2).
    #[serde(default)]
    create_tenants: Vec<String>,
    /// Grandfather existing tokens into a tenant (e.g. the shared bearer into fleet).
    #[serde(default)]
    imports: Vec<Imp>,
    /// Grandfather existing addresses to a tenant via register-owned.
    #[serde(default)]
    adopt: Vec<Adopt>,
    /// Phase-2: REASSIGN existing addresses to a new tenant via reassign-owned
    /// (unconditional owner-change; adopt/register-owned refuse a cross-tenant
    /// re-stamp). Same {address, tenant} shape as adopt.
    #[serde(default)]
    reassign: Vec<Adopt>,
}
#[derive(Deserialize)]
struct Lbl {
    name: String,
    value: String,
}
#[derive(Deserialize)]
struct Imp {
    tenant: String,
    token: String,
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    label: String,
}
#[derive(Deserialize)]
struct Adopt {
    address: String,
    tenant: String,
}

#[export(name = "theater:simple/actor.init")]
fn init(config: Value) -> Value {
    let raw = match config {
        Value::String(s) => s,
        _ => return abort("opsctl: init_state must be a JSON config string"),
    };
    let cfg: Cfg = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return abort(&format!("opsctl: bad config: {}", e)),
    };

    // 1. Store labels — the missing out-of-band seed/flip mechanism (Gap 1).
    for l in &cfg.labels {
        match store_at_label(STORE_ID.into(), l.name.clone(), l.value.clone().into_bytes()) {
            Ok(_) => log(format!("[opsctl] wrote store label '{}'", l.name)),
            Err(e) => return abort(&format!("opsctl: write label '{}' failed: {}", l.name, e)),
        }
    }

    // 2. create-tenant (Gap 2) — mint tenants + log their root tokens.
    for label in &cfg.create_tenants {
        if cfg.registry_id.is_empty() {
            return abort("opsctl: create_tenants requires registry_id");
        }
        let v = rpc_call(
            cfg.registry_id.clone(),
            String::from("theater:inbox/registry.create-tenant"),
            Value::Tuple(vec![Value::String(label.clone())]),
            Value::Tuple(vec![]),
        );
        match unwrap_result(v) {
            Ok(Value::Record { fields, .. }) => {
                let tid = record_field(&fields, "tenant_id");
                let tok = record_field(&fields, "root_token");
                // opsctl does its work in init, but the supervisor attaches its
                // monitor AFTER spawn (spawn auto-runs init), so init-time log
                // events are never captured — `supervisor chain` is empty for a
                // one-shot (supervisor-dev's finding). So PERSIST the token to a
                // store label: readable after exit, timing-independent, prod-true
                // (the flip consumes tokens from a durable place, not a chain dump).
                // Value = "<tenant_id> <root_token>".
                let out_label = format!("opsctl-created-{}", label);
                match store_at_label(STORE_ID.into(), out_label.clone(), format!("{} {}", tid, tok).into_bytes()) {
                    Ok(_) => log(format!(
                        "[opsctl] created tenant '{}' id={} -> token persisted to store label '{}'",
                        label, tid, out_label
                    )),
                    Err(e) => return abort(&format!(
                        "created tenant '{}' (id={}) but FAILED to persist its token to '{}': {}",
                        label, tid, out_label, e
                    )),
                }
            }
            Ok(other) => return abort(&bad_shape("create-tenant", other)),
            Err(e) => return abort(&format!("opsctl: create-tenant '{}' failed: {}", label, e)),
        }
    }

    // 3. import-key — grandfather existing tokens (e.g. the shared bearer -> fleet).
    for im in &cfg.imports {
        if cfg.registry_id.is_empty() {
            return abort("opsctl: imports requires registry_id");
        }
        let caps: Value = im.caps.clone().into();
        let v = rpc_call(
            cfg.registry_id.clone(),
            String::from("theater:inbox/registry.import-key"),
            Value::Tuple(vec![
                Value::String(im.tenant.clone()),
                Value::String(im.token.clone()),
                caps,
                Value::String(im.label.clone()),
            ]),
            Value::Tuple(vec![]),
        );
        match unwrap_result(v) {
            Ok(Value::String(kid)) => log(format!("[opsctl] imported key {} into tenant {}", kid, im.tenant)),
            Ok(other) => return abort(&bad_shape("import-key", other)),
            // Idempotent re-run: the registry enforces global token uniqueness, so a
            // second B3 run re-presenting the same token gets "token already registered".
            // The key is already fleet's — that's the desired end state, not a failure.
            Err(e) if e.contains("already registered") => log(format!(
                "[opsctl] token already imported into tenant {} (idempotent re-run) — continuing",
                im.tenant
            )),
            Err(e) => return abort(&format!("opsctl: import-key '{}' failed: {}", im.tenant, e)),
        }
    }

    // 4. adopt — stamp existing addresses to a tenant via register-owned.
    //
    // RESILIENT by design (supervisor-dev's belt-and-suspenders on top of the peel
    // fix): register-owned is idempotent for the same tenant (re-stamping an address
    // already owned by this tenant is a no-op Ok), so one odd return must NOT halt the
    // whole 26-address grandfather and leave a partial, unknowable state. We record
    // each failure + CONTINUE, persist a summary to a store label (init-time chain logs
    // aren't captured — same reason create-tenant persists its token), and if anything
    // failed we exit done=false so the operator re-runs opsctl-b3 to finish the
    // stragglers. The manager independently verifies router state (list-by-tenant)
    // rather than trusting this return, so a partial adopt is caught and completable.
    let mut adopted = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for a in &cfg.adopt {
        if cfg.router_id.is_empty() {
            return abort("opsctl: adopt requires router_id");
        }
        let v = rpc_call(
            cfg.router_id.clone(),
            String::from("theater:inbox/router.register-owned"),
            Value::Tuple(vec![Value::String(a.address.clone()), Value::String(a.tenant.clone())]),
            Value::Tuple(vec![]),
        );
        match unwrap_result(v) {
            Ok(Value::String(_)) => {
                adopted += 1;
                log(format!("[opsctl] adopted {} -> tenant {}", a.address, a.tenant));
            }
            Ok(other) => {
                failures.push(format!("{} (bad shape: {})", a.address, stringify(other)));
                log(format!("[opsctl] adopt {} bad shape, continuing", a.address));
            }
            Err(e) => {
                failures.push(format!("{} ({})", a.address, e));
                log(format!("[opsctl] adopt {} FAILED: {}, continuing", a.address, e));
            }
        }
    }
    if !cfg.adopt.is_empty() {
        log(format!("[opsctl] adopt summary: {}/{} adopted", adopted, cfg.adopt.len()));
        let summary = if failures.is_empty() {
            format!("adopted {}/{} OK", adopted, cfg.adopt.len())
        } else {
            format!("adopted {}/{}; FAILURES: {}", adopted, cfg.adopt.len(), failures.join("; "))
        };
        let _ = store_at_label(
            STORE_ID.into(),
            String::from("opsctl-adopt-summary"),
            summary.into_bytes(),
        );
        if !failures.is_empty() {
            log(format!(
                "[opsctl] adopt INCOMPLETE: {} failed; re-run opsctl-b3 (idempotent)",
                failures.len()
            ));
            OpsState::set(OpsState { done: false });
            return ok_unit();
        }
    }

    // 5. reassign (phase-2): move existing addresses to a new tenant via the
    // unconditional reassign-owned (register-owned refuses cross-tenant re-stamps).
    // Resilient like adopt; reassign-owned is idempotent (re-stamping to the same
    // tenant is a no-op Ok), so a re-run safely finishes any stragglers.
    let mut reassigned = 0usize;
    let mut re_failures: Vec<String> = Vec::new();
    for a in &cfg.reassign {
        if cfg.router_id.is_empty() {
            return abort("opsctl: reassign requires router_id");
        }
        let v = rpc_call(
            cfg.router_id.clone(),
            String::from("theater:inbox/router.reassign-owned"),
            Value::Tuple(vec![Value::String(a.address.clone()), Value::String(a.tenant.clone())]),
            Value::Tuple(vec![]),
        );
        match unwrap_result(v) {
            Ok(Value::String(_)) => {
                reassigned += 1;
                log(format!("[opsctl] reassigned {} -> tenant {}", a.address, a.tenant));
            }
            Ok(other) => {
                re_failures.push(format!("{} (bad shape: {})", a.address, stringify(other)));
                log(format!("[opsctl] reassign {} bad shape, continuing", a.address));
            }
            Err(e) => {
                re_failures.push(format!("{} ({})", a.address, e));
                log(format!("[opsctl] reassign {} FAILED: {}, continuing", a.address, e));
            }
        }
    }
    if !cfg.reassign.is_empty() {
        let n = cfg.reassign.len();
        log(format!("[opsctl] reassign summary: {}/{} reassigned", reassigned, n));
        let summary = if re_failures.is_empty() {
            format!("reassigned {}/{} OK", reassigned, n)
        } else {
            format!("reassigned {}/{}; FAILURES: {}", reassigned, n, re_failures.join("; "))
        };
        let _ = store_at_label(
            STORE_ID.into(),
            String::from("opsctl-reassign-summary"),
            summary.into_bytes(),
        );
        if !re_failures.is_empty() {
            log(format!("[opsctl] reassign INCOMPLETE: {} failed; re-run", re_failures.len()));
            OpsState::set(OpsState { done: false });
            return ok_unit();
        }
    }

    log(String::from("[opsctl] setup complete"));
    OpsState::set(OpsState { done: true });
    ok_unit()
}
