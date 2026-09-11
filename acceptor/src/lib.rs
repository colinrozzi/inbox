//! Inbox acceptor.
//!
//! On startup: parses initial_state (JSON config with bearer + DKIM + 4
//! sub-manifest references), persists secrets into the shared store, spawns the
//! singleton mailbox-router and smtp-acceptor, binds the HTTP listener. On each
//! TCP connection: spawns an api-handler and hands off the connection.
//!
//! in-module-state model (packr 0.24): AcceptorState lives in a #[derive(State)]
//! cell; exports no longer thread state. runtime->self; supervisor.spawn returns
//! result<string, supervisor-error> (raw-Value decode); stop-child -> stop-actor.
//!
//! Expected initial_state (JSON string): { bearer_token, dkim_private_key,
//! listen_addr, api_handler_manifest, mailbox_manifest, router_manifest,
//! smtp_acceptor_manifest, smtp_handler_manifest }. Backward-compat: a
//! "<bearer>\n<DKIM PEM>" string uses built-in default manifest refs + :443.

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};
use serde::{Deserialize, Serialize};
use theater_guest::State;

packr_guest::setup_guest!();

#[derive(Clone, GraphValue, State)]
pub struct AcceptorState {
    pub listener_id: String,
    pub router_id: String,
    pub api_handler_manifest: String,
}

pack_types! {
    // supervisor error typedefs — VERBATIM from theater's supervisor.pact.
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
        theater:simple/tcp {
            listen: func(address: string) -> result<string, string>,
            transfer: func(connection-id: string, target-actor: string) -> result<_, string>,
            transfer-async: func(connection-id: string, target-actor: string) -> result<_, string>,
        }
        theater:simple/supervisor {
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, supervisor-error>,
            stop-actor: func(id: string) -> result<_, supervisor-error>,
        }
        theater:simple/store {
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:simple/actor.get-state: func() -> value,
        theater:simple/tcp-client.handle-connection: func(connection-id: string) -> result<_, string>,
    }
}

#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/tcp", name = "listen")]
fn tcp_listen(address: String) -> Result<String, String>;

#[import(module = "theater:simple/tcp", name = "transfer-async")]
fn tcp_transfer_async(connection_id: String, target_actor: String) -> Result<(), String>;

// supervisor.spawn / stop-actor: import RAW Value, decode Value::Result (outer)
// / Variant (inner err) — packr-abi 0.24.
#[import(module = "theater:simple/supervisor", name = "spawn")]
fn supervisor_spawn_raw(manifest: String, init_state: Option<Value>, wasm_bytes: Option<Vec<u8>>) -> Value;

#[import(module = "theater:simple/supervisor", name = "stop-actor")]
fn supervisor_stop_actor_raw(id: String) -> Value;

fn supervisor_spawn(manifest: String, init_state: Option<Value>) -> Result<String, String> {
    match supervisor_spawn_raw(manifest, init_state, None) {
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

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:443";
const DEFAULT_API_HANDLER_MANIFEST: &str = "/home/colin/work/actors/inbox/api-handler/manifest.toml";
const DEFAULT_MAILBOX_MANIFEST: &str = "/home/colin/work/actors/inbox/mailbox/manifest.toml";
const DEFAULT_ROUTER_MANIFEST: &str = "/home/colin/work/actors/inbox/mailbox-router/manifest.toml";
const DEFAULT_SMTP_ACCEPTOR_MANIFEST: &str = "/home/colin/work/actors/inbox/smtp-acceptor/manifest.toml";

const STORE_ID: &str = "inbox";
const DKIM_KEY_LABEL: &str = "dkim-key";
const BEARER_TOKEN_LABEL: &str = "api-bearer-token";

#[derive(Deserialize)]
struct Config {
    bearer_token: String,
    dkim_private_key: String,
    listen_addr: String,
    api_handler_manifest: String,
    mailbox_manifest: String,
    router_manifest: String,
    smtp_acceptor_manifest: String,
    smtp_handler_manifest: String,
}

#[derive(Serialize)]
struct SmtpInit<'a> {
    router_id: &'a str,
    smtp_handler_manifest: &'a str,
}

// ---- result-Value helpers (identical across all inbox actors) ----
fn ok_unit() -> Value {
    let u = Value::Tuple(vec![]);
    Value::Result { ok_type: u.infer_type(), err_type: ValueType::String, value: Ok(Box::new(u)) }
}
fn err_str(msg: &str) -> Value {
    let e = Value::String(String::from(msg));
    Value::Result { ok_type: Value::Tuple(vec![]).infer_type(), err_type: ValueType::String, value: Err(Box::new(e)) }
}

#[export(name = "theater:simple/actor.init")]
fn init(config: Value) -> Value {
    log(String::from("[inbox-acceptor] init"));

    let raw = match config {
        Value::String(s) if !s.is_empty() => s,
        _ => return err_str(
            "acceptor needs initial_state as a non-empty string (JSON config or legacy '<bearer>\\n<DKIM PEM>')",
        ),
    };

    let (
        bearer_token,
        dkim_private_key,
        listen_addr,
        api_handler_manifest,
        mailbox_manifest,
        router_manifest,
        smtp_acceptor_manifest,
        smtp_handler_manifest_opt,
    ) = if let Ok(cfg) = serde_json::from_str::<Config>(&raw) {
        if cfg.bearer_token.is_empty() {
            return err_str("bearer_token must be non-empty");
        }
        if cfg.dkim_private_key.is_empty() {
            return err_str("dkim_private_key must be non-empty");
        }
        if cfg.listen_addr.is_empty() {
            return err_str("listen_addr must be non-empty");
        }
        if cfg.api_handler_manifest.is_empty()
            || cfg.mailbox_manifest.is_empty()
            || cfg.router_manifest.is_empty()
            || cfg.smtp_acceptor_manifest.is_empty()
            || cfg.smtp_handler_manifest.is_empty()
        {
            return err_str("all five *_manifest references must be non-empty");
        }
        (
            cfg.bearer_token,
            cfg.dkim_private_key,
            cfg.listen_addr,
            cfg.api_handler_manifest,
            cfg.mailbox_manifest,
            cfg.router_manifest,
            cfg.smtp_acceptor_manifest,
            Some(cfg.smtp_handler_manifest),
        )
    } else {
        match raw.split_once('\n') {
            Some((t, rest)) if !t.is_empty() => (
                t.to_string(),
                rest.to_string(),
                String::from(DEFAULT_LISTEN_ADDR),
                String::from(DEFAULT_API_HANDLER_MANIFEST),
                String::from(DEFAULT_MAILBOX_MANIFEST),
                String::from(DEFAULT_ROUTER_MANIFEST),
                String::from(DEFAULT_SMTP_ACCEPTOR_MANIFEST),
                None,
            ),
            _ => return err_str(
                "initial_state is neither valid JSON config nor legacy '<bearer-token>\\n<DKIM PEM>' shape",
            ),
        }
    };

    if let Err(e) = store_store_at_label(STORE_ID.into(), BEARER_TOKEN_LABEL.into(), bearer_token.into_bytes()) {
        return err_str(&format!("persist bearer token failed: {}", e));
    }
    if let Err(e) = store_store_at_label(STORE_ID.into(), DKIM_KEY_LABEL.into(), dkim_private_key.into_bytes()) {
        return err_str(&format!("persist dkim key failed: {}", e));
    }

    // Spawn the mailbox-router (owns address -> mailbox mapping; lazy-spawns).
    let router_id = match supervisor_spawn(router_manifest, Some(Value::String(mailbox_manifest))) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("spawn router failed: {}", e)),
    };
    log(format!("[inbox-acceptor] spawned mailbox-router {}", router_id));

    let listener_id = match tcp_listen(listen_addr.clone()) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("listen failed: {}", e)),
    };
    log(format!("[inbox-acceptor] HTTP listening on {} (id={})", listen_addr, listener_id));

    // Spawn the SMTP acceptor. JSON config -> pass {router_id, smtp_handler_manifest};
    // legacy -> pass the plain router id (smtp-acceptor uses its own default).
    let smtp_init_state = match &smtp_handler_manifest_opt {
        Some(handler_ref) => match serde_json::to_string(&SmtpInit {
            router_id: &router_id,
            smtp_handler_manifest: handler_ref,
        }) {
            Ok(s) => Value::String(s),
            Err(e) => return err_str(&format!("serialize smtp-acceptor init failed: {}", e)),
        },
        None => Value::String(router_id.clone()),
    };
    let smtp_acceptor_id = match supervisor_spawn(smtp_acceptor_manifest, Some(smtp_init_state)) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("spawn smtp-acceptor failed: {}", e)),
    };
    log(format!("[inbox-acceptor] spawned smtp-acceptor {}", smtp_acceptor_id));

    AcceptorState::set(AcceptorState { listener_id, router_id, api_handler_manifest });
    ok_unit()
}

#[export(name = "theater:simple/tcp-client.handle-connection")]
fn handle_connection(connection_id: String) -> Value {
    // Always Ok: a single failing connection must not kill the acceptor (that
    // would fail the whole supervision tree). Log + clean up + carry on.
    if let Err(e) = try_handle_connection(&connection_id) {
        log(format!("[inbox-acceptor] handle-connection failed (conn={}): {}", connection_id, e));
    }
    ok_unit()
}

fn try_handle_connection(connection_id: &str) -> Result<(), String> {
    let (api_handler_manifest, router_id) =
        AcceptorState::with(|s| (s.api_handler_manifest.clone(), s.router_id.clone()));

    let handler_id = supervisor_spawn(api_handler_manifest, Some(Value::String(router_id)))
        .map_err(|e| format!("spawn api-handler failed: {}", e))?;

    // Non-blocking hand-off (avoids the accept-loop wedge). On sync failure the
    // handler got no connection -> best-effort stop it.
    if let Err(e) = tcp_transfer_async(connection_id.to_string(), handler_id.clone()) {
        let _ = supervisor_stop_actor_raw(handler_id);
        return Err(format!("transfer-async failed: {}", e));
    }
    Ok(())
}
