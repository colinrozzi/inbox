//! Mailbox router: maps email addresses to mailbox actor IDs.
//!
//! Long-lived singleton. Addresses must be `register`ed before mail routes to
//! them (clean blast radius). in-module-state model (packr 0.24): RouterState
//! lives in a `#[derive(State)]` cell (small + serializable → the derive gives
//! the auto get-state export); exports no longer thread state. Bindings persist
//! under the store label `router-bindings`.
//!
//! LAZY SPAWN: on init we load the saved addresses but do NOT eager-spawn their
//! mailboxes (eager nested spawn during the router's own auto-init wedged the
//! theater supervisor — see the lazy-spawn hedge). Each mailbox is spawned on
//! its first lookup/register instead (a single spawn from an RPC context). An
//! empty `mailbox_id` means "known address, not yet spawned this process".

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value, ValueType};
use theater_guest::State;

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";
const BINDINGS_LABEL: &str = "router-bindings";

// forward_compatible: Binding is persisted (save_bindings) — keep field-add
// tolerance so a future Binding field is rollback-safe. crate= dropped (0.24).
#[derive(Clone, GraphValue)]
#[graph(forward_compatible)]
pub struct Binding {
    pub address: String,
    pub mailbox_id: String,
}

// RouterState in the derive(State) cell: small + serializable. The derive emits
// the actor.get-state export (declared below).
#[derive(Clone, GraphValue, State)]
pub struct RouterState {
    /// The manifest reference used to spawn new mailbox actors.
    pub mailbox_manifest: String,
    pub bindings: Vec<Binding>,
}

pack_types! {
    // supervisor error typedefs — VERBATIM from theater's supervisor.pact; the
    // interface hash resolves these, so byte-identical or spawn fails.
    variant spawn-failure {
        bad-manifest(string), wasm-fetch(string), handler-registry(string), wasm-invalid(string),
        interface-mismatch(string), missing-interface(string), missing-metadata(string), init-failed(string),
        child-failed(string), child-stopped(string), timeout(string), internal(string),
    }
    variant supervisor-error {
        actor-not-found(string), out-of-view(string), permission-denied(string), invalid-argument(string),
        spawn-failed(spawn-failure), runtime-unavailable, internal(string),
    }
    imports {
        theater:simple/self {
            log: func(msg: string),
        }
        theater:simple/supervisor {
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, supervisor-error>,
        }
        theater:simple/store {
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:simple/actor.get-state: func() -> value,
        theater:inbox/router.register: func(address: string) -> result<string, string>,
        theater:inbox/router.lookup: func(address: string) -> result<option<string>, string>,
        theater:inbox/router.list: func() -> result<list<binding>, string>,
    }
}

#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);

// supervisor.spawn: import RAW Value + decode Value::Result (outer) / Variant
// (inner err) — packr-abi 0.24. NOT Value::Variant on the outer wrapper.
#[import(module = "theater:simple/supervisor", name = "spawn")]
fn supervisor_spawn_raw(manifest: String, init_state: Option<Value>, wasm_bytes: Option<Vec<u8>>) -> Value;

fn supervisor_spawn(manifest: &str, init_state: Option<Value>) -> Result<String, String> {
    match supervisor_spawn_raw(String::from(manifest), init_state, None) {
        Value::Result { value: Ok(ok), .. } => match *ok {
            Value::String(id) => Ok(id),
            _ => Err(String::from("spawn: bad ok payload")),
        },
        Value::Result { value: Err(err), .. } => {
            let case = match *err {
                Value::Variant { case_name, .. } => case_name,
                _ => String::from("unknown"),
            };
            Err(format!("supervisor-error: {}", case))
        }
        _ => Err(String::from("spawn: unexpected result format")),
    }
}

#[import(module = "theater:simple/store", name = "get")]
fn store_get(store_id: String, content_ref: String) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/store", name = "get-by-label")]
fn store_get_by_label(store_id: String, label: String) -> Result<Option<String>, String>;

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

// ---- result-Value helpers (identical across all inbox actors) ----
fn ok_unit() -> Value {
    let u = Value::Tuple(vec![]);
    Value::Result { ok_type: u.infer_type(), err_type: ValueType::String, value: Ok(Box::new(u)) }
}
fn ok_result(v: Value) -> Value {
    Value::Result { ok_type: v.infer_type(), err_type: ValueType::String, value: Ok(Box::new(v)) }
}
fn err_str(msg: &str) -> Value {
    let e = Value::String(String::from(msg));
    Value::Result { ok_type: Value::Tuple(vec![]).infer_type(), err_type: ValueType::String, value: Err(Box::new(e)) }
}
// option<string> return payload. `.into()` is the GraphValue/packr conversion,
// consistent with Vec/struct .into() (confirming From<Option<String>> w/ theater-dev).
fn opt_string(o: Option<String>) -> Value {
    o.into()
}

fn load_bindings() -> Vec<Binding> {
    let content_ref = match store_get_by_label(STORE_ID.into(), String::from(BINDINGS_LABEL)) {
        Ok(Some(r)) => r,
        _ => return Vec::new(),
    };
    let bytes = match store_get(STORE_ID.into(), content_ref) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let value = match decode(&bytes) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    Vec::<Binding>::try_from(value).unwrap_or_else(|_| Vec::new())
}

fn save_bindings(bindings: &[Binding]) {
    let value: Value = bindings.to_vec().into();
    match encode(&value) {
        Ok(bytes) => {
            if let Err(e) = store_store_at_label(STORE_ID.into(), String::from(BINDINGS_LABEL), bytes) {
                log(format!("[mailbox-router] persist failed: {}", e));
            }
        }
        Err(e) => log(format!("[mailbox-router] encode failed: {:?}", e)),
    }
}

fn spawn_mailbox(manifest: &str, address: &str) -> Result<String, String> {
    supervisor_spawn(manifest, Some(Value::String(String::from(address))))
        .map_err(|e| format!("spawn mailbox failed: {}", e))
}

#[export(name = "theater:simple/actor.init")]
fn init(config: Value) -> Value {
    let mailbox_manifest = match config {
        Value::String(s) => s,
        _ => return err_str("mailbox-router init: expected init_state = string (mailbox manifest path)"),
    };
    log(format!("[mailbox-router] init (manifest={}) — lazy mailbox spawn", mailbox_manifest));
    // Keep the known addresses, but clear mailbox_id (not-spawned-this-process).
    let bindings: Vec<Binding> = load_bindings()
        .into_iter()
        .map(|b| Binding { address: b.address, mailbox_id: String::new() })
        .collect();
    RouterState::set(RouterState { mailbox_manifest, bindings });
    ok_unit()
}

/// Position + current mailbox_id of `address`, if known. Read-only snapshot.
fn find(address: &str) -> Option<(usize, String)> {
    RouterState::with(|s| {
        s.bindings
            .iter()
            .position(|b| b.address == address)
            .map(|idx| (idx, s.bindings[idx].mailbox_id.clone()))
    })
}

/// Lazily spawn the mailbox for a known address (idx) if not yet spawned this
/// process; record + return its id. Spawn is a host call done OUTSIDE the cell
/// borrow, then the id is written back.
fn ensure_spawned(idx: usize, address: &str) -> Result<String, String> {
    let manifest = RouterState::with(|s| s.mailbox_manifest.clone());
    let id = spawn_mailbox(&manifest, address)?;
    log(format!("[mailbox-router] lazily spawned {} -> {}", address, id));
    RouterState::with_mut(|s| s.bindings[idx].mailbox_id = id.clone());
    Ok(id)
}

#[export(name = "theater:inbox/router.register")]
fn register(address: String) -> Value {
    match find(&address) {
        Some((_, id)) if !id.is_empty() => ok_result(Value::String(id)),
        Some((idx, _)) => match ensure_spawned(idx, &address) {
            Ok(id) => ok_result(Value::String(id)),
            Err(e) => err_str(&e),
        },
        None => {
            let manifest = RouterState::with(|s| s.mailbox_manifest.clone());
            match spawn_mailbox(&manifest, &address) {
                Ok(id) => {
                    log(format!("[mailbox-router] registered {} -> {}", address, id));
                    RouterState::with_mut(|s| {
                        s.bindings.push(Binding { address: address.clone(), mailbox_id: id.clone() });
                    });
                    RouterState::with(|s| save_bindings(&s.bindings));
                    ok_result(Value::String(id))
                }
                Err(e) => err_str(&e),
            }
        }
    }
}

#[export(name = "theater:inbox/router.lookup")]
fn lookup(address: String) -> Value {
    match find(&address) {
        None => ok_result(opt_string(None)),
        Some((_, id)) if !id.is_empty() => ok_result(opt_string(Some(id))),
        Some((idx, _)) => match ensure_spawned(idx, &address) {
            Ok(id) => ok_result(opt_string(Some(id))),
            // Spawn failure on a known address: report not-found rather than error
            // (matches the pre-migration behavior — lookup never hard-failed).
            Err(_) => ok_result(opt_string(None)),
        },
    }
}

#[export(name = "theater:inbox/router.list")]
fn list() -> Value {
    let bindings = RouterState::with(|s| s.bindings.clone());
    ok_result(bindings.into())
}
