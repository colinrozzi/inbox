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
            spawn: func(manifest: string, init-bytes: option<list<u8>>, wasm-bytes: option<list<u8>>) -> result<string, string>,
        }
        theater:simple/rpc {
            call: func(actor-id: string, function: string, params: value, options: value) -> value,
        }
        theater:simple/store {
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value, mailbox-manifest: string) -> result<router-state, string>,
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
    init_bytes: Option<Vec<u8>>,
    wasm_bytes: Option<Vec<u8>>,
) -> Result<String, String>;

#[import(module = "theater:simple/rpc", name = "call")]
fn rpc_call(actor_id: String, function: String, params: Value, options: Value) -> Value;

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

/// Spawn a mailbox actor and pass it the address so it can load its own
/// messages from the store. Returns the new actor id.
fn spawn_mailbox(manifest: &str, address: &str) -> Result<String, String> {
    let mailbox_id = supervisor_spawn(String::from(manifest), None, None)
        .map_err(|e| format!("spawn mailbox failed: {}", e))?;
    let init_params = Value::Tuple(alloc::vec![Value::String(String::from(address))]);
    let _ = rpc_call(
        mailbox_id.clone(),
        String::from("theater:simple/actor.init"),
        init_params,
        Value::Tuple(alloc::vec![]),
    );
    Ok(mailbox_id)
}

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value, mailbox_manifest: String) -> Result<(RouterState, ()), String> {
    log(format!("[mailbox-router] init (manifest={})", mailbox_manifest));

    let saved = load_bindings();
    let mut bindings: Vec<Binding> = Vec::with_capacity(saved.len());
    for b in &saved {
        match spawn_mailbox(&mailbox_manifest, &b.address) {
            Ok(new_id) => {
                log(format!(
                    "[mailbox-router] restored {} -> {} (was {})",
                    b.address, new_id, b.mailbox_id
                ));
                bindings.push(Binding {
                    address: b.address.clone(),
                    mailbox_id: new_id,
                });
            }
            Err(e) => log(format!(
                "[mailbox-router] failed to restore {}: {}",
                b.address, e
            )),
        }
    }

    if !bindings.is_empty() {
        save_bindings(&bindings);
    }

    let _ = ValueType::Bool; // silence unused
    Ok((
        RouterState {
            mailbox_manifest,
            bindings,
        },
        (),
    ))
}

#[export(name = "theater:inbox/router.register")]
fn register(state: RouterState, address: String) -> Result<(RouterState, String), String> {
    let mut state = state;

    if let Some(b) = state.bindings.iter().find(|b| b.address == address) {
        // Idempotent: registering an existing address returns its current id.
        let mailbox_id = b.mailbox_id.clone();
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
    let id = state
        .bindings
        .iter()
        .find(|b| b.address == address)
        .map(|b| b.mailbox_id.clone());
    Ok((state, id))
}

#[export(name = "theater:inbox/router.list")]
fn list(state: RouterState) -> Result<(RouterState, Vec<Binding>), String> {
    let bindings = state.bindings.clone();
    Ok((state, bindings))
}
