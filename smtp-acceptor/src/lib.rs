//! SMTP acceptor: listens on TCP for inbound mail. Same actor-per-connection
//! pattern as the HTTP acceptor — spawns an smtp-handler for each connection,
//! passes it the mailbox ID, transfers.
//!
//! Init state shape (Value::String): either
//!   {"router_id": "<id>", "smtp_handler_manifest": "<reference>"}  — JSON
//! or
//!   "<router_id>"                                                  — legacy
//!
//! Legacy shape is what the pre-refactor inbox-acceptor sends (plain
//! router id, no manifest reference); on that path we fall back to the
//! built-in DEFAULT_SMTP_HANDLER_MANIFEST file path which is what the
//! systemd VPS deploy lays out via the nix store. New inbox-acceptor
//! sends the JSON shape with a deploy-agnostic manifest reference.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};
use serde::Deserialize;

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
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, string>,
            stop-child: func(child-id: string) -> result<_, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<smtp-acceptor-state, string>,
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
    init_state: Option<Value>,
    wasm_bytes: Option<Vec<u8>>,
) -> Result<String, String>;

#[import(module = "theater:simple/supervisor", name = "stop-child")]
fn supervisor_stop_child(child_id: String) -> Result<(), String>;

const LISTEN_ADDR: &str = "0.0.0.0:25";

// Default smtp-handler manifest reference — used only on the legacy
// init path (plain Value::String router id, no JSON). Once cutover
// happens and every spawn is JSON with smtp_handler_manifest supplied
// by inbox-acceptor's config, this becomes unused.
const DEFAULT_SMTP_HANDLER_MANIFEST: &str =
    "/home/colin/work/actors/inbox/smtp-handler/manifest.toml";

#[derive(Deserialize)]
struct Config {
    router_id: String,
    smtp_handler_manifest: String,
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(SmtpAcceptorState, ()), String> {
    let raw = match state {
        Value::String(s) if !s.is_empty() => s,
        _ => {
            return Err(String::from(
                "smtp-acceptor init: expected init_state as a non-empty string \
                 (JSON {router_id, smtp_handler_manifest} or legacy plain router id)",
            ))
        }
    };

    let (router_id, smtp_handler_manifest) =
        if let Ok(cfg) = serde_json::from_str::<Config>(&raw) {
            if cfg.router_id.is_empty() {
                return Err(String::from("router_id must be non-empty"));
            }
            if cfg.smtp_handler_manifest.is_empty() {
                return Err(String::from("smtp_handler_manifest must be non-empty"));
            }
            (cfg.router_id, cfg.smtp_handler_manifest)
        } else {
            // Legacy: bare router id string. Default the manifest reference
            // to the systemd-deploy file path.
            (raw, String::from(DEFAULT_SMTP_HANDLER_MANIFEST))
        };

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
            smtp_handler_manifest,
        },
        (),
    ))
}

#[export(name = "theater:simple/tcp-client.handle-connection")]
fn handle_connection(
    state: SmtpAcceptorState,
    connection_id: String,
) -> Result<(SmtpAcceptorState, ()), String> {
    // Always-Ok: see the acceptor for context. A connection error must
    // not kill the smtp listener.
    if let Err(e) = try_handle_connection(&state, &connection_id) {
        log(format!(
            "[inbox-smtp-acceptor] handle-connection failed (conn={}): {}",
            connection_id, e
        ));
    }
    let _ = ValueType::Bool; // suppress unused
    Ok((state, ()))
}

fn try_handle_connection(state: &SmtpAcceptorState, connection_id: &str) -> Result<(), String> {
    // supervisor.spawn now does setup+auto-init: the router id we pass as
    // init_state is delivered to smtp-handler's init synchronously inside
    // the spawn call; the returned handler_id is post-init.
    let init_state = Some(Value::String(state.router_id.clone()));
    let handler_id = supervisor_spawn(state.smtp_handler_manifest.clone(), init_state, None)
        .map_err(|e| format!("spawn smtp-handler failed: {}", e))?;

    if let Err(e) = tcp_transfer(connection_id.to_string(), handler_id.clone()) {
        let _ = supervisor_stop_child(handler_id);
        return Err(format!("transfer failed: {}", e));
    }
    Ok(())
}
