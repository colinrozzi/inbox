//! SMTP acceptor: listens on :25 for inbound mail. Actor-per-connection —
//! spawns an smtp-handler per connection and hands off (non-blocking).
//!
//! in-module-state model (packr 0.24): SmtpAcceptorState in a #[derive(State)]
//! cell; exports drop state; runtime->self; runtime.spawn returns
//! result<string, runtime-error> (raw-Value decode); spawn/stop via runtime (post-#204).
//!
//! Init state (Value::String): JSON {router_id, smtp_handler_manifest} or a
//! legacy plain "<router_id>" (falls back to DEFAULT_SMTP_HANDLER_MANIFEST).

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use base64::engine::{general_purpose::STANDARD as B64, Engine as _};
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};
use serde::Deserialize;
use theater_guest::State;

packr_guest::setup_guest!();

#[derive(Clone, GraphValue, State)]
pub struct SmtpAcceptorState {
    pub listener_id: String,
    pub router_id: String,
    pub smtp_handler_manifest: String,
    /// smtp-handler wasm bytes for the bare-box push-spawn, threaded into each
    /// per-connection smtp-handler spawn. Empty on the fleet instance (init omits it)
    /// => runtime.spawn resolves manifest.package. Init-provided; not store-persisted.
    pub smtp_handler_wasm: Vec<u8>,
}

pack_types! {
    // runtime error typedefs — VERBATIM from theater's runtime pact (post-#204,
    // c3937bdc: supervisor handler dissolved into runtime). Byte-identical or
    // spawn fails. (actor-info omitted — list-actors isn't imported.)
    variant spawn-failure {
        bad-manifest(string), wasm-fetch(string), handler-registry(string), wasm-invalid(string),
        interface-mismatch(string), missing-interface(string), missing-metadata(string), init-failed(string),
        child-failed(string), child-stopped(string), timeout(string), internal(string),
    }
    variant runtime-error {
        permission-denied(string), runtime-unavailable, actor-not-found(string), invalid-argument(string),
        spawn-failed(spawn-failure), internal(string),
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
        theater:simple/runtime {
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, runtime-error>,
            stop-actor: func(id: string) -> result<_, runtime-error>,
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

#[import(module = "theater:simple/runtime", name = "spawn")]
fn supervisor_spawn_raw(manifest: String, init_state: Option<Value>, wasm_bytes: Option<Vec<u8>>) -> Value;

#[import(module = "theater:simple/runtime", name = "stop-actor")]
fn supervisor_stop_actor_raw(id: String) -> Value;

fn supervisor_spawn(manifest: String, init_state: Option<Value>, wasm: Option<Vec<u8>>) -> Result<String, String> {
    match supervisor_spawn_raw(manifest, init_state, wasm) {
        Value::Result { value: Ok(ok), .. } => match *ok {
            Value::String(id) => Ok(id),
            _ => Err(String::from("spawn: bad ok payload")),
        },
        Value::Result { value: Err(err), .. } => {
            let case = match *err {
                Value::Variant { case_name, .. } => case_name,
                _ => String::from("unknown"),
            };
            Err(format!("runtime-error: {}", case))
        }
        _ => Err(String::from("spawn: unexpected result format")),
    }
}

const LISTEN_ADDR: &str = "0.0.0.0:25";
const DEFAULT_SMTP_HANDLER_MANIFEST: &str = "/home/colin/work/actors/inbox/smtp-handler/manifest.toml";

#[derive(Deserialize)]
struct Config {
    router_id: String,
    smtp_handler_manifest: String,
    // SMTP bind address. Empty => the LISTEN_ADDR default (0.0.0.0:25). Lets a
    // non-root dev/proof node bind an unprivileged port (e.g. 0.0.0.0:2525)
    // from init config, no source patch; prod omits it and gets :25.
    #[serde(default)]
    listen_addr: String,
    // smtp-handler wasm (base64) for the bare-box push-spawn, threaded into each
    // per-connection handler spawn. "" on the fleet instance => package resolution.
    #[serde(default)]
    smtp_handler_wasm_b64: String,
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
    let raw = match config {
        Value::String(s) if !s.is_empty() => s,
        _ => return err_str(
            "smtp-acceptor init: expected init_state as a non-empty string (JSON {router_id, smtp_handler_manifest} or legacy plain router id)",
        ),
    };

    let (router_id, smtp_handler_manifest, listen_addr, smtp_handler_wasm_b64) = if let Ok(cfg) = serde_json::from_str::<Config>(&raw) {
        if cfg.router_id.is_empty() {
            return err_str("router_id must be non-empty");
        }
        if cfg.smtp_handler_manifest.is_empty() {
            return err_str("smtp_handler_manifest must be non-empty");
        }
        (cfg.router_id, cfg.smtp_handler_manifest, cfg.listen_addr, cfg.smtp_handler_wasm_b64)
    } else {
        (raw, String::from(DEFAULT_SMTP_HANDLER_MANIFEST), String::new(), String::new())
    };

    // Decode the pushed smtp-handler wasm (bare box); empty on the fleet => package.
    let smtp_handler_wasm = if smtp_handler_wasm_b64.is_empty() {
        Vec::new()
    } else {
        match B64.decode(smtp_handler_wasm_b64.as_bytes()) {
            Ok(b) => b,
            Err(e) => return err_str(&format!("smtp_handler_wasm_b64 decode failed: {}", e)),
        }
    };

    // Empty listen_addr => the :25 default (prod). A dev/proof node sets it to an
    // unprivileged port so a non-root spin-up needs no source patch.
    let listen = if listen_addr.trim().is_empty() {
        String::from(LISTEN_ADDR)
    } else {
        listen_addr
    };

    log(format!("[inbox-smtp-acceptor] init (router={})", router_id));

    let listener_id = match tcp_listen(listen.clone()) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("listen failed: {}", e)),
    };
    log(format!("[inbox-smtp-acceptor] SMTP listening on {} (id={})", listen, listener_id));

    SmtpAcceptorState::set(SmtpAcceptorState { listener_id, router_id, smtp_handler_manifest, smtp_handler_wasm });
    ok_unit()
}

#[export(name = "theater:simple/tcp-client.handle-connection")]
fn handle_connection(connection_id: String) -> Value {
    // Always-Ok: a connection error must not kill the :25 listener.
    if let Err(e) = try_handle_connection(&connection_id) {
        log(format!("[inbox-smtp-acceptor] handle-connection failed (conn={}): {}", connection_id, e));
    }
    ok_unit()
}

fn try_handle_connection(connection_id: &str) -> Result<(), String> {
    let (router_id, manifest, handler_wasm) = SmtpAcceptorState::with(|s| {
        (s.router_id.clone(), s.smtp_handler_manifest.clone(), s.smtp_handler_wasm.clone())
    });

    // Thread the pushed smtp-handler wasm on a bare box; empty => None => package.
    let wasm = if handler_wasm.is_empty() { None } else { Some(handler_wasm) };
    let handler_id = supervisor_spawn(manifest, Some(Value::String(router_id)), wasm)
        .map_err(|e| format!("spawn smtp-handler failed: {}", e))?;

    // Non-blocking hand-off — a slow/stalled/malicious client can't serialize +
    // wedge the accept loop and take :25 down (the accept-loop wedge root cause).
    if let Err(e) = tcp_transfer_async(connection_id.to_string(), handler_id.clone()) {
        let _ = supervisor_stop_actor_raw(handler_id);
        return Err(format!("transfer-async failed: {}", e));
    }
    Ok(())
}
