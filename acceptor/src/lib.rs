//! Inbox acceptor.
//!
//! On startup: parses initial_state (JSON config of NON-SECRET manifest refs),
//! VERIFIES the bearer + DKIM secrets are already present in the shared store
//! (gatekeeper — see below), spawns the singleton mailbox-router and
//! smtp-acceptor, binds the HTTP listener. On each TCP connection: spawns an
//! api-handler and hands off the connection.
//!
//! in-module-state model (packr 0.24): AcceptorState lives in a #[derive(State)]
//! cell; exports no longer thread state. runtime->self; runtime.spawn returns
//! result<string, runtime-error> (raw-Value decode); spawn/stop via runtime (post-#204).
//!
//! SECRETS ARE STORE-SEEDED, NOT MANIFEST-CARRIED. The acceptor no longer OWNS
//! bearer/dkim: they are seeded into the on-box store out-of-band (manager-owned,
//! never over an http-published manifest). At init the acceptor is a READER +
//! GATEKEEPER — it verifies both secret labels resolve to non-empty content and
//! refuses to bring up the listener otherwise, so a missing/unseeded secret fails
//! LOUD here at the singleton entry point rather than later inside a per-connection
//! api-handler. This keeps EVERY manifest secret-free and http-publishable.
//!
//! Expected initial_state (JSON string): { listen_addr, api_handler_manifest,
//! mailbox_manifest, router_manifest, smtp_acceptor_manifest, smtp_handler_manifest }.

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
    /// tenant-registry actor id, "" if not wired. Passed to each api-handler.
    pub registry_id: String,
    pub api_handler_manifest: String,
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
        theater:simple/store {
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
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
#[import(module = "theater:simple/runtime", name = "spawn")]
fn supervisor_spawn_raw(manifest: String, init_state: Option<Value>, wasm_bytes: Option<Vec<u8>>) -> Value;

#[import(module = "theater:simple/runtime", name = "stop-actor")]
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
            Err(format!("runtime-error: {}", case))
        }
        _ => Err(String::from("spawn: unexpected result format")),
    }
}

// Read-side store bindings. The acceptor no longer WRITES secrets (store-at-label
// dropped) — the manager seeds them out-of-band; the acceptor only verifies them.
#[import(module = "theater:simple/store", name = "get-by-label")]
fn store_get_by_label(store_id: String, label: String) -> Result<Option<String>, String>;

#[import(module = "theater:simple/store", name = "get")]
fn store_get(store_id: String, content_ref: String) -> Result<Vec<u8>, String>;

const STORE_ID: &str = "inbox";
const DKIM_KEY_LABEL: &str = "dkim-key";
const BEARER_TOKEN_LABEL: &str = "api-bearer-token";

// initial_state config — NON-SECRET only (bearer/dkim live in the store, seeded
// out-of-band by the manager). This makes the acceptor manifest http-publishable.
#[derive(Deserialize)]
struct Config {
    listen_addr: String,
    api_handler_manifest: String,
    mailbox_manifest: String,
    router_manifest: String,
    smtp_acceptor_manifest: String,
    smtp_handler_manifest: String,
    // Multi-tenancy: the tenant-registry manifest. Optional — absent leaves the
    // registry unspawned and api-handler enforcement permanently off (inert).
    #[serde(default)]
    tenant_registry_manifest: String,
}

#[derive(Serialize)]
struct SmtpInit<'a> {
    router_id: &'a str,
    smtp_handler_manifest: &'a str,
}

#[derive(Serialize)]
struct ApiHandlerInit<'a> {
    router_id: &'a str,
    registry_id: &'a str,
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

// GATEKEEPER: a secret must resolve to non-empty content in the store, or the
// acceptor refuses to start. Mirrors the #75 store guard, but for a SECRET both
// "absent" and "present-but-unreadable" are fatal — unlike the mailbox, absent is
// NOT a valid "fresh" state (you cannot serve mail without bearer/dkim). Secrets
// are stored as plain bytes (into_bytes, no CGRF), so presence + non-empty is the
// whole check; the acceptor itself never uses the values (downstream actors read
// them from the store), so we verify and discard.
fn require_secret(label: &str) -> Result<(), String> {
    match store_get_by_label(STORE_ID.into(), label.into()) {
        Err(e) => Err(format!("store get-by-label('{}') failed: {}", label, e)),
        Ok(None) => Err(format!(
            "secret label '{}' is ABSENT from the store — refusing to start; the manager \
             must seed bearer/dkim into the on-box store before first acceptor start",
            label
        )),
        Ok(Some(content_ref)) => match store_get(STORE_ID.into(), content_ref) {
            Err(e) => Err(format!("store get for secret '{}' failed: {}", label, e)),
            Ok(bytes) if bytes.is_empty() => Err(format!(
                "secret label '{}' is present but EMPTY — refusing to start",
                label
            )),
            Ok(_) => Ok(()),
        },
    }
}

#[export(name = "theater:simple/actor.init")]
fn init(config: Value) -> Value {
    log(String::from("[inbox-acceptor] init"));

    let raw = match config {
        Value::String(s) if !s.is_empty() => s,
        _ => return err_str("acceptor needs initial_state as a non-empty JSON config string"),
    };

    let cfg: Config = match serde_json::from_str::<Config>(&raw) {
        Ok(cfg) => cfg,
        Err(e) => return err_str(&format!("initial_state is not valid JSON config: {}", e)),
    };
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
    let Config {
        listen_addr,
        api_handler_manifest,
        mailbox_manifest,
        router_manifest,
        smtp_acceptor_manifest,
        smtp_handler_manifest,
        tenant_registry_manifest,
    } = cfg;

    // GATEKEEPER: secrets must already be seeded in the store (manager-owned, never
    // in this http-publishable manifest). Fail LOUD if absent/empty/unreadable —
    // never bring up the listener over missing bearer/dkim.
    if let Err(e) = require_secret(BEARER_TOKEN_LABEL) {
        return err_str(&e);
    }
    if let Err(e) = require_secret(DKIM_KEY_LABEL) {
        return err_str(&e);
    }
    log(String::from(
        "[inbox-acceptor] secrets verified present in store (bearer + dkim) — gatekeeper ok",
    ));

    // Spawn the mailbox-router (owns address -> mailbox mapping; lazy-spawns).
    let router_id = match supervisor_spawn(router_manifest, Some(Value::String(mailbox_manifest))) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("spawn router failed: {}", e)),
    };
    log(format!("[inbox-acceptor] spawned mailbox-router {}", router_id));

    // Spawn the tenant-registry singleton (multi-tenancy). Optional: an empty
    // manifest leaves registry_id empty, so api-handler enforcement can never
    // engage — the acceptor stays fully backward-compatible / inert.
    let registry_id = if tenant_registry_manifest.is_empty() {
        String::new()
    } else {
        match supervisor_spawn(tenant_registry_manifest, Some(Value::String(String::new()))) {
            Ok(id) => {
                log(format!("[inbox-acceptor] spawned tenant-registry {}", id));
                id
            }
            Err(e) => return err_str(&format!("spawn tenant-registry failed: {}", e)),
        }
    };

    let listener_id = match tcp_listen(listen_addr.clone()) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("listen failed: {}", e)),
    };
    log(format!("[inbox-acceptor] HTTP listening on {} (id={})", listen_addr, listener_id));

    // Spawn the SMTP acceptor with {router_id, smtp_handler_manifest}.
    let smtp_init_state = match serde_json::to_string(&SmtpInit {
        router_id: &router_id,
        smtp_handler_manifest: &smtp_handler_manifest,
    }) {
        Ok(s) => Value::String(s),
        Err(e) => return err_str(&format!("serialize smtp-acceptor init failed: {}", e)),
    };
    let smtp_acceptor_id = match supervisor_spawn(smtp_acceptor_manifest, Some(smtp_init_state)) {
        Ok(id) => id,
        Err(e) => return err_str(&format!("spawn smtp-acceptor failed: {}", e)),
    };
    log(format!("[inbox-acceptor] spawned smtp-acceptor {}", smtp_acceptor_id));

    AcceptorState::set(AcceptorState { listener_id, router_id, registry_id, api_handler_manifest });
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
    let (api_handler_manifest, router_id, registry_id) = AcceptorState::with(|s| {
        (s.api_handler_manifest.clone(), s.router_id.clone(), s.registry_id.clone())
    });

    // Pass both ids as JSON. api-handler tolerates a bare router_id too (legacy).
    let init = serde_json::to_string(&ApiHandlerInit { router_id: &router_id, registry_id: &registry_id })
        .map_err(|e| format!("serialize api-handler init failed: {}", e))?;
    let handler_id = supervisor_spawn(api_handler_manifest, Some(Value::String(init)))
        .map_err(|e| format!("spawn api-handler failed: {}", e))?;

    // Non-blocking hand-off (avoids the accept-loop wedge). On sync failure the
    // handler got no connection -> best-effort stop it.
    if let Err(e) = tcp_transfer_async(connection_id.to_string(), handler_id.clone()) {
        let _ = supervisor_stop_actor_raw(handler_id);
        return Err(format!("transfer-async failed: {}", e));
    }
    Ok(())
}
