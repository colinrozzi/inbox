//! SMTP acceptor: listens on TCP for inbound mail. Same actor-per-connection
//! pattern as the HTTP acceptor — spawns an smtp-handler for each connection,
//! passes it the mailbox ID, transfers.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};

packr_guest::setup_guest!();

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct SmtpAcceptorState {
    pub listener_id: String,
    pub router_id: String,
    pub smtp_handler_manifest: String,
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
        }
        theater:simple/rpc {
            call: func(actor-id: string, function: string, params: value, options: value) -> value,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value, router-id: string) -> result<smtp-acceptor-state, string>,
        theater:simple/tcp-client.handle-connection: func(state: smtp-acceptor-state, connection-id: string) -> result<smtp-acceptor-state, string>,
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

#[import(module = "theater:simple/rpc", name = "call")]
fn rpc_call(actor_id: String, function: String, params: Value, options: Value) -> Value;

const LISTEN_ADDR: &str = "0.0.0.0:25";
const SMTP_HANDLER_MANIFEST: &str = "/home/colin/work/actors/inbox/smtp-handler/manifest.toml";

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value, router_id: String) -> Result<(SmtpAcceptorState, ()), String> {
    log(format!("[inbox-smtp-acceptor] init (router={})", router_id));

    let listener_id = tcp_listen(String::from(LISTEN_ADDR))
        .map_err(|e| format!("listen failed: {}", e))?;
    log(format!(
        "[inbox-smtp-acceptor] SMTP listening on {} (id={})",
        LISTEN_ADDR, listener_id
    ));

    Ok((
        SmtpAcceptorState {
            listener_id,
            router_id,
            smtp_handler_manifest: String::from(SMTP_HANDLER_MANIFEST),
        },
        (),
    ))
}

#[export(name = "theater:simple/tcp-client.handle-connection")]
fn handle_connection(
    state: SmtpAcceptorState,
    connection_id: String,
) -> Result<(SmtpAcceptorState, ()), String> {
    let handler_id = supervisor_spawn(state.smtp_handler_manifest.clone(), None, None)
        .map_err(|e| format!("spawn smtp-handler failed: {}", e))?;

    // Pass router_id to the handler via init params.
    let init_params = Value::Tuple(alloc::vec![Value::String(state.router_id.clone())]);
    let _ = rpc_call(
        handler_id.clone(),
        String::from("theater:simple/actor.init"),
        init_params,
        Value::Tuple(alloc::vec![]),
    );

    tcp_transfer(connection_id, handler_id).map_err(|e| format!("transfer failed: {}", e))?;

    // Suppress dead_code warning for ValueType import used only when adding optional fields.
    let _ = ValueType::Bool;
    Ok((state, ()))
}
