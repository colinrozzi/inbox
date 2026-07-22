//! Mailbox router: maps email addresses to mailbox actor IDs.
//!
//! The router is a long-lived singleton. Each registered address has its
//! own mailbox actor. Lookups are explicit — addresses must be registered
//! via `register` before mail can be delivered to them; this gives us a
//! clean blast radius (random spam can't spawn unbounded mailboxes).
//!
//! State persistence: the bindings list lives under the store label
//! `router-bindings`. The router's own actor id is fresh per process, and
//! so are every mailbox's actor id — so on init we eagerly re-spawn a
//! mailbox actor for each saved address, hand it the address so it can
//! load its own messages from the store, and rebuild the bindings list
//! with the new ids.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value, ValueType};

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";
const BINDINGS_LABEL: &str = "router-bindings";

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct Binding {
    pub address: String,
    pub mailbox_id: String,
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct RouterState {
    /// The manifest path used to spawn new mailbox actors.
    pub mailbox_manifest: String,
    pub bindings: Vec<Binding>,
}

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
        }
        theater:simple/supervisor {
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, string>,
        }
        theater:simple/store {
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<router-state, string>,
        theater:inbox/router.register: func(state: router-state, address: string) -> result<tuple<router-state, string>, string>,
        theater:inbox/router.lookup: func(state: router-state, address: string) -> result<tuple<router-state, option<string>>, string>,
        theater:inbox/router.list: func(state: router-state) -> result<tuple<router-state, list<binding>>, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/supervisor", name = "spawn")]
fn supervisor_spawn(
    manifest: String,
    init_state: Option<Value>,
    wasm_bytes: Option<Vec<u8>>,
) -> Result<String, String>;

#[import(module = "theater:simple/store", name = "get")]
fn store_get(store_id: String, content_ref: String) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/store", name = "get-by-label")]
fn store_get_by_label(store_id: String, label: String) -> Result<Option<String>, String>;

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

fn load_bindings() -> Vec<Binding> {
    let label = String::from(BINDINGS_LABEL);
    let content_ref = match store_get_by_label(STORE_ID.into(), label) {
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
            if let Err(e) = store_store_at_label(
                STORE_ID.into(),
                String::from(BINDINGS_LABEL),
                bytes,
            ) {
                log(format!("[mailbox-router] persist failed: {}", e));
            }
        }
        Err(e) => log(format!("[mailbox-router] encode failed: {:?}", e)),
    }
}

/// Spawn a mailbox actor with its address packed into init_state so the
/// new actor can load its own messages from the store on init. Auto-init
/// runs synchronously inside `supervisor_spawn`, so the id is only
/// returned after the child has finished its init.
fn spawn_mailbox(manifest: &str, address: &str) -> Result<String, String> {
    let init_state = Some(Value::String(String::from(address)));
    supervisor_spawn(String::from(manifest), init_state, None)
        .map_err(|e| format!("spawn mailbox failed: {}", e))
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(RouterState, ()), String> {
    let mailbox_manifest = match state {
        Value::String(s) => s,
        _ => return Err(String::from(
            "mailbox-router init: expected init_state = string (mailbox manifest path)",
        )),
    };
    log(format!("[mailbox-router] init (manifest={}) — lazy mailbox spawn", mailbox_manifest));

    // LAZY SPAWN (do NOT eager-restore mailboxes here). Eager, synchronous
    // multi-spawn during the router's OWN auto-init triggers a theater
    // supervisor spin on the 2nd nested spawn (the router is itself being
    // spawned synchronously by the acceptor; spawning a 2nd child from inside
    // that auto-init wedges). So we keep the known addresses but spawn each
    // mailbox on its FIRST lookup instead (a single spawn from a normal RPC
    // context, not nested-in-init). That lets the acceptor's spawn(router)
    // return -> the spine binds. Mailbox actor ids are fresh per process
    // anyway, so the persisted mailbox_id is not authoritative; an empty
    // mailbox_id here means "known address, not yet spawned this process".
    let saved = load_bindings();
    let bindings: Vec<Binding> = saved
        .into_iter()
        .map(|b| Binding {
            address: b.address,
            mailbox_id: String::new(),
        })
        .collect();

    let _ = ValueType::Bool; // silence unused
    Ok((
        RouterState {
            mailbox_manifest,
            bindings,
        },
        (),
    ))
}

/// Ensure the mailbox for `bindings[idx]` is spawned in THIS process; spawn it
/// lazily if not (empty mailbox_id). Returns its current actor id. A single
/// spawn from an RPC context (lookup/register) — not the eager-in-init path.
fn ensure_spawned(state: &mut RouterState, idx: usize) -> Result<String, String> {
    if state.bindings[idx].mailbox_id.is_empty() {
        let address = state.bindings[idx].address.clone();
        let id = spawn_mailbox(&state.mailbox_manifest, &address)?;
        log(format!("[mailbox-router] lazily spawned {} -> {}", address, id));
        state.bindings[idx].mailbox_id = id.clone();
        Ok(id)
    } else {
        Ok(state.bindings[idx].mailbox_id.clone())
    }
}

#[export(name = "theater:inbox/router.register")]
fn register(state: RouterState, address: String) -> Result<(RouterState, String), String> {
    let mut state = state;

    if let Some(idx) = state.bindings.iter().position(|b| b.address == address) {
        // Idempotent: known address. Ensure its mailbox is spawned this process
        // (lazy spawn if it hasn't been looked up yet) and return the id.
        let mailbox_id = ensure_spawned(&mut state, idx)?;
        return Ok((state, mailbox_id));
    }

    let mailbox_id = spawn_mailbox(&state.mailbox_manifest, &address)?;

    log(format!(
        "[mailbox-router] registered {} -> {}",
        address, mailbox_id
    ));
    state.bindings.push(Binding {
        address,
        mailbox_id: mailbox_id.clone(),
    });
    save_bindings(&state.bindings);
    Ok((state, mailbox_id))
}

#[export(name = "theater:inbox/router.lookup")]
fn lookup(state: RouterState, address: String) -> Result<(RouterState, Option<String>), String> {
    let mut state = state;
    match state.bindings.iter().position(|b| b.address == address) {
        // Known address: spawn its mailbox lazily on first lookup this process.
        Some(idx) => {
            let id = ensure_spawned(&mut state, idx).ok();
            Ok((state, id))
        }
        None => Ok((state, None)),
    }
}

#[export(name = "theater:inbox/router.list")]
fn list(state: RouterState) -> Result<(RouterState, Vec<Binding>), String> {
    let bindings = state.bindings.clone();
    Ok((state, bindings))
}
