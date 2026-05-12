//! Inbox acceptor.
//!
//! On startup: spawns the singleton mailbox actor, holds onto its ID.
//! On each TCP connection: spawns an api-handler, hands it the mailbox ID
//! via init bytes, then transfers the connection.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};

packr_guest::setup_guest!();

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct AcceptorState {
    pub listener_id: String,
    pub router_id: String,
    pub api_handler_manifest: String,
}

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
        }
        theater:simple/tcp {
            listen: func(address: string) -> result<string, string>,
            transfer: func(connection-id: string, target-actor: string) -> result<_, string>,
        }
        theater:simple/supervisor {
            spawn: func(manifest: string, init-bytes: option<list<u8>>, wasm-bytes: option<list<u8>>) -> result<string, string>,
            stop-child: func(child-id: string) -> result<_, string>,
        }
        theater:simple/rpc {
            call: func(actor-id: string, function: string, params: value, options: value) -> value,
        }
        theater:simple/store {
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<acceptor-state, string>,
        theater:simple/tcp-client.handle-connection: func(state: acceptor-state, connection-id: string) -> result<acceptor-state, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/tcp", name = "listen")]
fn tcp_listen(address: String) -> Result<String, String>;

#[import(module = "theater:simple/tcp", name = "transfer")]
fn tcp_transfer(connection_id: String, target_actor: String) -> Result<(), String>;

#[import(module = "theater:simple/supervisor", name = "spawn")]
fn supervisor_spawn(
    manifest: String,
    init_bytes: Option<Vec<u8>>,
    wasm_bytes: Option<Vec<u8>>,
) -> Result<String, String>;

#[import(module = "theater:simple/supervisor", name = "stop-child")]
fn supervisor_stop_child(child_id: String) -> Result<(), String>;

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

#[import(module = "theater:simple/rpc", name = "call")]
fn rpc_call(actor_id: String, function: String, params: Value, options: Value) -> Value;

const LISTEN_ADDR: &str = "0.0.0.0:8080";
const API_HANDLER_MANIFEST: &str = "/home/colin/work/actors/inbox/api-handler/manifest.toml";
const MAILBOX_MANIFEST: &str = "/home/colin/work/actors/inbox/mailbox/manifest.toml";
const ROUTER_MANIFEST: &str = "/home/colin/work/actors/inbox/mailbox-router/manifest.toml";
const SMTP_ACCEPTOR_MANIFEST: &str = "/home/colin/work/actors/inbox/smtp-acceptor/manifest.toml";

const STORE_ID: &str = "inbox";
const DKIM_KEY_LABEL: &str = "dkim-key";
const BEARER_TOKEN_LABEL: &str = "api-bearer-token";

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(AcceptorState, ()), String> {
    log(String::from("[inbox-acceptor] init"));

    // initial_state format: first line is the API bearer token, the rest is
    // the DKIM private key (PEM). Both go into the shared store under
    // stable labels so api-handler children can fetch them on demand.
    let raw = match state {
        Value::String(s) if !s.is_empty() => s,
        _ => {
            return Err(String::from(
                "acceptor needs initial_state = \"<bearer-token>\\n<DKIM PEM>\" in manifest",
            ))
        }
    };
    let (bearer_token, dkim_private_key_pem) = match raw.split_once('\n') {
        Some((t, rest)) if !t.is_empty() => (t.to_string(), rest.to_string()),
        _ => {
            return Err(String::from(
                "initial_state must be: <bearer-token>\\n<DKIM PEM>",
            ))
        }
    };
    store_store_at_label(
        String::from(STORE_ID),
        String::from(BEARER_TOKEN_LABEL),
        bearer_token.into_bytes(),
    )
    .map_err(|e| format!("persist bearer token failed: {}", e))?;
    store_store_at_label(
        String::from(STORE_ID),
        String::from(DKIM_KEY_LABEL),
        dkim_private_key_pem.into_bytes(),
    )
    .map_err(|e| format!("persist dkim key failed: {}", e))?;

    // Spawn the mailbox-router. It owns the address → mailbox-actor mapping
    // and spawns mailbox actors on demand.
    let router_id = supervisor_spawn(String::from(ROUTER_MANIFEST), None, None)
        .map_err(|e| format!("spawn router failed: {}", e))?;
    log(format!("[inbox-acceptor] spawned mailbox-router {}", router_id));

    let init_params = Value::Tuple(alloc::vec![Value::String(String::from(MAILBOX_MANIFEST))]);
    let _ = rpc_call(
        router_id.clone(),
        String::from("theater:simple/actor.init"),
        init_params,
        Value::Tuple(alloc::vec![]),
    );

    let listener_id = tcp_listen(String::from(LISTEN_ADDR))
        .map_err(|e| format!("listen failed: {}", e))?;
    log(format!(
        "[inbox-acceptor] HTTP listening on {} (id={})",
        LISTEN_ADDR, listener_id
    ));

    // Spawn the SMTP acceptor and pass it the router ID so inbound mail
    // can be routed to the right mailbox.
    let smtp_acceptor_id =
        supervisor_spawn(String::from(SMTP_ACCEPTOR_MANIFEST), None, None)
            .map_err(|e| format!("spawn smtp-acceptor failed: {}", e))?;
    let smtp_init_params = Value::Tuple(alloc::vec![Value::String(router_id.clone())]);
    let _ = rpc_call(
        smtp_acceptor_id.clone(),
        String::from("theater:simple/actor.init"),
        smtp_init_params,
        Value::Tuple(alloc::vec![]),
    );
    log(format!(
        "[inbox-acceptor] spawned smtp-acceptor {}",
        smtp_acceptor_id
    ));

    let _ = ValueType::Bool; // silence unused if no other ValueType use
    Ok((
        AcceptorState {
            listener_id,
            router_id,
            api_handler_manifest: String::from(API_HANDLER_MANIFEST),
        },
        (),
    ))
}

#[export(name = "theater:simple/tcp-client.handle-connection")]
fn handle_connection(
    state: AcceptorState,
    connection_id: String,
) -> Result<(AcceptorState, ()), String> {
    // Always return Ok regardless of what happens inside. A single failing
    // connection (e.g. client closed before we could transfer — theater
    // returns "Connection not found" for that) must not kill the acceptor:
    // if it does, theater treats the whole supervision tree as failed and
    // the process exits. Log + clean up + carry on.
    if let Err(e) = try_handle_connection(&state, &connection_id) {
        log(format!(
            "[inbox-acceptor] handle-connection failed (conn={}): {}",
            connection_id, e
        ));
    }
    Ok((state, ()))
}

fn try_handle_connection(state: &AcceptorState, connection_id: &str) -> Result<(), String> {
    let handler_id = supervisor_spawn(state.api_handler_manifest.clone(), None, None)
        .map_err(|e| format!("spawn api-handler failed: {}", e))?;

    // Just router_id — api-handler loads the DKIM key from the store itself.
    let init_params = Value::Tuple(alloc::vec![Value::String(state.router_id.clone())]);
    let _ = rpc_call(
        handler_id.clone(),
        String::from("theater:simple/actor.init"),
        init_params,
        Value::Tuple(alloc::vec![]),
    );

    if let Err(e) = tcp_transfer(connection_id.to_string(), handler_id.clone()) {
        // Transfer failed — the api-handler is sitting there with no
        // connection to handle. Stop it so we don't leak actors.
        let _ = supervisor_stop_child(handler_id);
        return Err(format!("transfer failed: {}", e));
    }
    Ok(())
}
