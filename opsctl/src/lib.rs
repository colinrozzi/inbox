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

/// Peel the outer result<_, _> off an RPC return.
fn unwrap_result(v: Value) -> Option<Value> {
    match v {
        Value::Result { value: Ok(ok), .. } => Some(*ok),
        _ => None,
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
            Some(Value::Record { fields, .. }) => {
                let tid = record_field(&fields, "tenant_id");
                let tok = record_field(&fields, "root_token");
                log(format!("[opsctl] created tenant '{}' id={} root_token={}", label, tid, tok));
            }
            _ => return abort(&format!("opsctl: create-tenant '{}' failed", label)),
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
            Some(Value::String(kid)) => log(format!("[opsctl] imported key {} into tenant {}", kid, im.tenant)),
            _ => return abort(&format!("opsctl: import-key into '{}' failed", im.tenant)),
        }
    }

    // 4. adopt — stamp existing addresses to a tenant via register-owned.
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
            Some(Value::String(_)) => log(format!("[opsctl] adopted {} -> tenant {}", a.address, a.tenant)),
            _ => return abort(&format!("opsctl: register-owned '{}' failed", a.address)),
        }
    }

    log(String::from("[opsctl] setup complete"));
    OpsState::set(OpsState { done: true });
    ok_unit()
}
