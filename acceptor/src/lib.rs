//! Inbox acceptor.
//!
//! On startup: parses initial_state (JSON config with bearer + DKIM + 4
//! sub-manifest references), persists secrets into the shared store,
//! spawns the singleton mailbox-router and smtp-acceptor, binds :443.
//! On each TCP connection: spawns an api-handler (using the
//! api_handler_manifest reference held in AcceptorState), hands it the
//! router ID, then transfers the connection.
//!
//! Expected initial_state shape (JSON string in Value::String):
//!   {
//!     "bearer_token":           "<API bearer or comma-separated rotation list>",
//!     "dkim_private_key":       "<PEM, newlines as \\n escapes inside JSON>",
//!     "api_handler_manifest":   "<theater resolve_reference: file:/https:/store:>",
//!     "mailbox_manifest":       "<same>",
//!     "router_manifest":        "<same>",
//!     "smtp_acceptor_manifest": "<same>"
//!   }
//!
//! Backward-compat: if initial_state is NOT JSON, accept the legacy
//! "<bearer-line>\n<DKIM PEM>" shape and use built-in default file-path
//! references for the 4 sub-manifests (matching what the systemd
//! build_manifest.py script on the VPS currently emits). Keeps the
//! systemd path working through the refactor → cutover window.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value, ValueType};
use serde::{Deserialize, Serialize};

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
            spawn: func(manifest: string, init-state: option<value>, wasm-bytes: option<list<u8>>) -> result<string, string>,
            stop-child: func(child-id: string) -> result<_, string>,
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
    init_state: Option<Value>,
    wasm_bytes: Option<Vec<u8>>,
) -> Result<String, String>;

#[import(module = "theater:simple/supervisor", name = "stop-child")]
fn supervisor_stop_child(child_id: String) -> Result<(), String>;

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

const LISTEN_ADDR: &str = "0.0.0.0:443";

// Default sub-manifest references — used ONLY by the backward-compat
// branch of init() when initial_state is in the legacy line-prefix shape.
// The systemd build_manifest.py on the VPS currently emits that shape.
// Once move-inbox-under-sentinel cutover (roadmap item 2) happens, every
// deploy passes JSON initial_state and these defaults become unused.
const DEFAULT_API_HANDLER_MANIFEST: &str =
    "/home/colin/work/actors/inbox/api-handler/manifest.toml";
const DEFAULT_MAILBOX_MANIFEST: &str = "/home/colin/work/actors/inbox/mailbox/manifest.toml";
const DEFAULT_ROUTER_MANIFEST: &str =
    "/home/colin/work/actors/inbox/mailbox-router/manifest.toml";
const DEFAULT_SMTP_ACCEPTOR_MANIFEST: &str =
    "/home/colin/work/actors/inbox/smtp-acceptor/manifest.toml";

const STORE_ID: &str = "inbox";
const DKIM_KEY_LABEL: &str = "dkim-key";
const BEARER_TOKEN_LABEL: &str = "api-bearer-token";

#[derive(Deserialize)]
struct Config {
    bearer_token: String,
    dkim_private_key: String,
    api_handler_manifest: String,
    mailbox_manifest: String,
    router_manifest: String,
    smtp_acceptor_manifest: String,
    // smtp-acceptor (spawned at init) needs its own handler-manifest
    // reference to spawn smtp-handler per inbound SMTP connection.
    // We pass this through to smtp-acceptor via its init_state JSON.
    smtp_handler_manifest: String,
}

#[derive(Serialize)]
struct SmtpInit<'a> {
    router_id: &'a str,
    smtp_handler_manifest: &'a str,
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(AcceptorState, ()), String> {
    log(String::from("[inbox-acceptor] init"));

    let raw = match state {
        Value::String(s) if !s.is_empty() => s,
        _ => {
            return Err(String::from(
                "acceptor needs initial_state as a non-empty string \
                 (JSON config or legacy '<bearer>\\n<DKIM PEM>')",
            ))
        }
    };

    let (
        bearer_token,
        dkim_private_key,
        api_handler_manifest,
        mailbox_manifest,
        router_manifest,
        smtp_acceptor_manifest,
        // None means legacy path: smtp-acceptor will use its own default.
        // Some(ref) means JSON path: we pass this through to smtp-acceptor.
        smtp_handler_manifest_opt,
    ) = if let Ok(cfg) = serde_json::from_str::<Config>(&raw) {
        if cfg.bearer_token.is_empty() {
            return Err(String::from("bearer_token must be non-empty"));
        }
        if cfg.dkim_private_key.is_empty() {
            return Err(String::from("dkim_private_key must be non-empty"));
        }
        if cfg.api_handler_manifest.is_empty()
            || cfg.mailbox_manifest.is_empty()
            || cfg.router_manifest.is_empty()
            || cfg.smtp_acceptor_manifest.is_empty()
            || cfg.smtp_handler_manifest.is_empty()
        {
            return Err(String::from(
                "all five *_manifest references must be non-empty",
            ));
        }
        (
            cfg.bearer_token,
            cfg.dkim_private_key,
            cfg.api_handler_manifest,
            cfg.mailbox_manifest,
            cfg.router_manifest,
            cfg.smtp_acceptor_manifest,
            Some(cfg.smtp_handler_manifest),
        )
    } else {
        // Backward-compat: legacy "<bearer-line>\n<DKIM PEM>" shape used by
        // the systemd build_manifest.py on the VPS. Default the four
        // sub-manifest references to the hardcoded file paths that shape
        // implicitly relies on. smtp_handler_manifest is None — smtp-acceptor
        // receives a plain router_id string and uses its own default.
        match raw.split_once('\n') {
            Some((t, rest)) if !t.is_empty() => (
                t.to_string(),
                rest.to_string(),
                String::from(DEFAULT_API_HANDLER_MANIFEST),
                String::from(DEFAULT_MAILBOX_MANIFEST),
                String::from(DEFAULT_ROUTER_MANIFEST),
                String::from(DEFAULT_SMTP_ACCEPTOR_MANIFEST),
                None,
            ),
            _ => {
                return Err(String::from(
                    "initial_state is neither valid JSON config nor legacy \
                     '<bearer-token>\\n<DKIM PEM>' shape",
                ))
            }
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
        dkim_private_key.into_bytes(),
    )
    .map_err(|e| format!("persist dkim key failed: {}", e))?;

    // Spawn the mailbox-router. It owns the address → mailbox-actor mapping
    // and spawns mailbox actors on demand. supervisor.spawn now does
    // setup+auto-init: init_state is passed straight to the child's init.
    let router_id = supervisor_spawn(
        router_manifest,
        Some(Value::String(mailbox_manifest)),
        None,
    )
    .map_err(|e| format!("spawn router failed: {}", e))?;
    log(format!("[inbox-acceptor] spawned mailbox-router {}", router_id));

    let listener_id = tcp_listen(String::from(LISTEN_ADDR))
        .map_err(|e| format!("listen failed: {}", e))?;
    log(format!(
        "[inbox-acceptor] HTTP listening on {} (id={})",
        LISTEN_ADDR, listener_id
    ));

    // Spawn the SMTP acceptor. When we have a smtp_handler_manifest
    // reference from JSON config, pass a JSON {router_id, smtp_handler_manifest}
    // so smtp-acceptor can spawn smtp-handler via a deploy-agnostic
    // reference. On the legacy path, fall back to passing just the
    // plain router id string — smtp-acceptor uses its own default
    // file-path reference for the handler.
    let smtp_init_state = match &smtp_handler_manifest_opt {
        Some(handler_ref) => Value::String(
            serde_json::to_string(&SmtpInit {
                router_id: &router_id,
                smtp_handler_manifest: handler_ref,
            })
            .map_err(|e| format!("serialize smtp-acceptor init failed: {}", e))?,
        ),
        None => Value::String(router_id.clone()),
    };
    let smtp_acceptor_id = supervisor_spawn(
        smtp_acceptor_manifest,
        Some(smtp_init_state),
        None,
    )
    .map_err(|e| format!("spawn smtp-acceptor failed: {}", e))?;
    log(format!(
        "[inbox-acceptor] spawned smtp-acceptor {}",
        smtp_acceptor_id
    ));

    let _ = ValueType::Bool; // silence unused if no other ValueType use
    Ok((
        AcceptorState {
            listener_id,
            router_id,
            api_handler_manifest,
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
    // supervisor.spawn does setup+auto-init: the router id we pass here is
    // delivered to api-handler's init synchronously; it also pulls the
    // DKIM key and bearer token from the store on its own.
    let handler_id = supervisor_spawn(
        state.api_handler_manifest.clone(),
        Some(Value::String(state.router_id.clone())),
        None,
    )
    .map_err(|e| format!("spawn api-handler failed: {}", e))?;

    if let Err(e) = tcp_transfer(connection_id.to_string(), handler_id.clone()) {
        // Transfer failed — the api-handler is sitting there with no
        // connection to handle. Stop it so we don't leak actors.
        let _ = supervisor_stop_child(handler_id);
        return Err(format!("transfer failed: {}", e));
    }
    Ok(())
}
