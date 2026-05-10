//! Mailbox router: maps email addresses to mailbox actor IDs.
//!
//! The router is a long-lived singleton. Each registered address has its
//! own mailbox actor. Lookups are explicit — addresses must be registered
//! via `register` before mail can be delivered to them; this gives us a
//! clean blast radius (random spam can't spawn unbounded mailboxes).

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};

packr_guest::setup_guest!();

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

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value, mailbox_manifest: String) -> Result<(RouterState, ()), String> {
    log(format!("[mailbox-router] init (manifest={})", mailbox_manifest));
    Ok((
        RouterState {
            mailbox_manifest,
            bindings: Vec::new(),
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

    let mailbox_id = supervisor_spawn(state.mailbox_manifest.clone(), None, None)
        .map_err(|e| format!("spawn mailbox failed: {}", e))?;

    // Initialize the mailbox.
    let init_params = Value::Tuple(alloc::vec![Value::Option {
        inner_type: ValueType::List(alloc::boxed::Box::new(ValueType::U8)),
        value: None,
    }]);
    let _ = rpc_call(
        mailbox_id.clone(),
        String::from("theater:simple/actor.init"),
        init_params,
        Value::Tuple(alloc::vec![]),
    );

    log(format!(
        "[mailbox-router] registered {} -> {}",
        address, mailbox_id
    ));
    state.bindings.push(Binding {
        address,
        mailbox_id: mailbox_id.clone(),
    });
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
