//! Inbox API handler: per-connection HTTP handler.
//!
//! Receives one HTTP request, routes it, calls into the mailbox actor via
//! RPC, returns JSON, closes the connection, and shuts itself down.
//!
//! Routes:
//!   GET  /v1/mailboxes                              → list registered addresses
//!   POST /v1/mailboxes                              → register an address
//!   GET  /v1/mailboxes/<addr>                       → look up the mailbox record
//!   GET  /v1/mailboxes/<addr>/inbox?since=<n>       → list messages since cursor n
//!   POST /v1/mailboxes/<addr>/messages              → store a message
//!   POST /v1/mailboxes/<addr>/send                  → send a message from this address

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

// The rsa crate pulls in getrandom transitively even though our PKCS#1 v1.5
// signing path is deterministic. Wire a custom backend that errors — it's
// never actually called along the sign() codepath.
getrandom::register_custom_getrandom!(unsupported_getrandom);
fn unsupported_getrandom(_dest: &mut [u8]) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}

mod dkim;
mod rfc2822;

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct HandlerState {
    pub router_id: String,
    pub dkim_private_key_pem: String,
    pub bearer_token: String,
}

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
            shutdown: func(data: option<list<u8>>) -> result<_, string>,
        }
        theater:simple/tcp {
            connect: func(address: string) -> result<string, string>,
            receive: func(connection-id: string, max-bytes: u32) -> result<list<u8>, string>,
            send: func(connection-id: string, data: list<u8>) -> result<u64, string>,
            close: func(connection-id: string) -> result<_, string>,
            upgrade-to-tls-client: func(connection-id: string, server-name: string) -> result<_, string>,
        }
        theater:simple/rpc {
            call: func(actor-id: string, function: string, params: value, options: value) -> value,
        }
        theater:simple/store {
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
        }
        theater:simple/timer {
            now: func() -> u64,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value, router-id: string) -> result<handler-state, string>,
        theater:simple/tcp-client.handle-connection-transfer: func(state: handler-state, connection-id: string) -> result<handler-state, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/runtime", name = "shutdown")]
fn shutdown(data: Option<Vec<u8>>) -> Result<(), String>;

#[import(module = "theater:simple/tcp", name = "connect")]
fn tcp_connect(address: String) -> Result<String, String>;

#[import(module = "theater:simple/tcp", name = "receive")]
fn tcp_receive(connection_id: String, max_bytes: u32) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/tcp", name = "send")]
fn tcp_send(connection_id: String, data: Vec<u8>) -> Result<u64, String>;

#[import(module = "theater:simple/tcp", name = "close")]
fn tcp_close(connection_id: String) -> Result<(), String>;

#[import(module = "theater:simple/tcp", name = "upgrade-to-tls-client")]
fn tcp_upgrade_to_tls_client(connection_id: String, server_name: String) -> Result<(), String>;

#[import(module = "theater:simple/rpc", name = "call")]
fn rpc_call(actor_id: String, function: String, params: Value, options: Value) -> Value;

#[import(module = "theater:simple/store", name = "get")]
fn store_get(store_id: String, content_ref: String) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/store", name = "get-by-label")]
fn store_get_by_label(store_id: String, label: String) -> Result<Option<String>, String>;

#[import(module = "theater:simple/timer", name = "now")]
fn timer_now() -> u64;

const STORE_ID: &str = "inbox";
const DKIM_KEY_LABEL: &str = "dkim-key";
const BEARER_TOKEN_LABEL: &str = "api-bearer-token";

fn load_label_as_string(label: &str) -> Result<String, String> {
    let content_ref = store_get_by_label(String::from(STORE_ID), String::from(label))
        .map_err(|e| format!("{} lookup failed: {}", label, e))?
        .ok_or_else(|| format!("{} label not set (acceptor should have written it)", label))?;
    let bytes = store_get(String::from(STORE_ID), content_ref)
        .map_err(|e| format!("{} get failed: {}", label, e))?;
    String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", label))
}

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value, router_id: String) -> Result<(HandlerState, ()), String> {
    let dkim_private_key_pem = load_label_as_string(DKIM_KEY_LABEL)?;
    let bearer_token = load_label_as_string(BEARER_TOKEN_LABEL)?;
    Ok((
        HandlerState {
            router_id,
            dkim_private_key_pem,
            bearer_token,
        },
        (),
    ))
}

#[export(name = "theater:simple/tcp-client.handle-connection-transfer")]
fn handle_connection_transfer(
    state: HandlerState,
    connection_id: String,
) -> Result<(HandlerState, ()), String> {
    let request = tcp_receive(connection_id.clone(), 65536).unwrap_or_default();

    let response = route(
        &request,
        &state.router_id,
        &state.dkim_private_key_pem,
        &state.bearer_token,
    );

    if let Err(e) = tcp_send(connection_id.clone(), response) {
        log(format!("[inbox-api] send failed: {}", e));
    }
    let _ = tcp_close(connection_id);
    let _ = shutdown(None);

    Ok((state, ()))
}

// ============================================================================
// Routing
// ============================================================================

fn route(
    request: &[u8],
    router_id: &str,
    dkim_private_key_pem: &str,
    bearer_token: &str,
) -> Vec<u8> {
    let request_str = match core::str::from_utf8(request) {
        Ok(s) => s,
        Err(_) => return http_response(400, "application/json", br#"{"error":"non-utf8 request"}"#.to_vec()),
    };

    if extract_bearer(request_str) != Some(bearer_token) {
        return http_response(401, "application/json", br#"{"error":"unauthorized"}"#.to_vec());
    }

    let first_line = request_str.lines().next().unwrap_or("");
    let mut parts = first_line.split(' ');
    let method = parts.next().unwrap_or("");
    let path_and_query = parts.next().unwrap_or("/");
    let (path, query) = match path_and_query.find('?') {
        Some(i) => (&path_and_query[..i], &path_and_query[i + 1..]),
        None => (path_and_query, ""),
    };

    // POST /v1/mailboxes — register a new address.
    if method == "POST" && path == "/v1/mailboxes" {
        return handle_register_mailbox(request_str, router_id);
    }

    // GET /v1/mailboxes — list registered addresses.
    if method == "GET" && path == "/v1/mailboxes" {
        return handle_list_mailboxes(router_id);
    }

    // /v1/mailboxes/<address>/...
    if let Some(rest) = path.strip_prefix("/v1/mailboxes/") {
        // Split address from any sub-path.
        let (address_enc, sub) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let address = url_decode(address_enc);

        // Resolve the address → mailbox id via the router.
        let mailbox_id = match resolve_mailbox(router_id, &address) {
            Ok(Some(id)) => id,
            Ok(None) => {
                return http_response(
                    404,
                    "application/json",
                    format!(r#"{{"error":"unknown address: {}"}}"#, json_escape(&address))
                        .into_bytes(),
                );
            }
            Err(e) => {
                return http_response(
                    500,
                    "application/json",
                    format!(r#"{{"error":"router rpc failed: {}"}}"#, json_escape(&e))
                        .into_bytes(),
                );
            }
        };

        return match (method, sub) {
            ("GET", "") => {
                let body = format!(
                    r#"{{"address":"{}","mailbox_id":"{}"}}"#,
                    json_escape(&address),
                    json_escape(&mailbox_id)
                );
                http_response(200, "application/json", body.into_bytes())
            }
            ("GET", "inbox") => handle_inbox(query, &mailbox_id),
            ("POST", "messages") => handle_post_message(request_str, &mailbox_id),
            ("POST", "send") => handle_send(request_str, &address, dkim_private_key_pem),
            _ => http_response(404, "application/json", br#"{"error":"not found"}"#.to_vec()),
        };
    }

    http_response(404, "application/json", br#"{"error":"not found"}"#.to_vec())
}

/// `POST /v1/mailboxes` — register a new address.
fn handle_register_mailbox(request_str: &str, router_id: &str) -> Vec<u8> {
    let body = match request_str.find("\r\n\r\n") {
        Some(i) => &request_str[i + 4..],
        None => return http_response(400, "application/json", br#"{"error":"missing body"}"#.to_vec()),
    };
    let parsed = parse_simple_json_object(body);
    let address = parsed.iter().find(|(k, _)| k == "address").map(|(_, v)| v.clone()).unwrap_or_default();
    if address.is_empty() {
        return http_response(400, "application/json", br#"{"error":"address is required"}"#.to_vec());
    }

    let result = rpc_call(
        router_id.to_string(),
        String::from("theater:inbox/router.register"),
        Value::Tuple(alloc::vec![Value::String(address.clone())]),
        Value::Tuple(alloc::vec![]),
    );
    let mailbox_id = match unwrap_rpc_result(result) {
        Some(Value::String(id)) => id,
        _ => return http_response(500, "application/json", br#"{"error":"router rpc failed"}"#.to_vec()),
    };

    let body = format!(
        r#"{{"address":"{}","mailbox_id":"{}"}}"#,
        json_escape(&address),
        json_escape(&mailbox_id)
    );
    http_response(201, "application/json", body.into_bytes())
}

/// `GET /v1/mailboxes` — list all registered addresses.
fn handle_list_mailboxes(router_id: &str) -> Vec<u8> {
    let result = rpc_call(
        router_id.to_string(),
        String::from("theater:inbox/router.list"),
        Value::Tuple(alloc::vec![]),
        Value::Tuple(alloc::vec![]),
    );
    let bindings = match unwrap_rpc_result(result) {
        Some(Value::List { items, .. }) => items,
        _ => return http_response(500, "application/json", br#"{"error":"router rpc failed"}"#.to_vec()),
    };
    let parts: Vec<String> = bindings
        .iter()
        .map(|b| match b {
            Value::Record { fields, .. } => {
                let addr = fields
                    .iter()
                    .find(|(k, _)| k == "address")
                    .and_then(|(_, v)| if let Value::String(s) = v { Some(s.clone()) } else { None })
                    .unwrap_or_default();
                format!(r#""{}""#, json_escape(&addr))
            }
            _ => String::from(r#""""#),
        })
        .collect();
    let body = format!(r#"{{"mailboxes":[{}]}}"#, parts.join(","));
    http_response(200, "application/json", body.into_bytes())
}

/// Look up a mailbox actor ID by address through the router.
fn resolve_mailbox(router_id: &str, address: &str) -> Result<Option<String>, String> {
    let result = rpc_call(
        router_id.to_string(),
        String::from("theater:inbox/router.lookup"),
        Value::Tuple(alloc::vec![Value::String(address.to_string())]),
        Value::Tuple(alloc::vec![]),
    );
    match unwrap_rpc_result(result) {
        Some(Value::Option { value: Some(inner), .. }) => match *inner {
            Value::String(id) => Ok(Some(id)),
            _ => Err(String::from("unexpected router response shape")),
        },
        Some(Value::Option { value: None, .. }) => Ok(None),
        _ => Err(String::from("router rpc failed")),
    }
}

fn handle_inbox(query: &str, mailbox_id: &str) -> Vec<u8> {
    let cursor = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("since="))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let result = rpc_call(
        mailbox_id.to_string(),
        String::from("theater:inbox/mailbox.list-since"),
        Value::Tuple(alloc::vec![Value::U64(cursor)]),
        Value::Tuple(alloc::vec![]),
    );

    let page = match unwrap_rpc_result(result) {
        Some(p) => p,
        None => return http_response(500, "application/json", br#"{"error":"mailbox rpc failed"}"#.to_vec()),
    };
    let json = inbox_page_to_json(&page);
    http_response(200, "application/json", json.into_bytes())
}

fn handle_post_message(request_str: &str, mailbox_id: &str) -> Vec<u8> {
    let body = match request_str.find("\r\n\r\n") {
        Some(i) => &request_str[i + 4..],
        None => return http_response(400, "application/json", br#"{"error":"missing body"}"#.to_vec()),
    };

    // Tiny JSON object parser — supports flat string-valued objects only.
    let parsed = parse_simple_json_object(body);
    let from = parsed.iter().find(|(k, _)| k == "from").map(|(_, v)| v.clone()).unwrap_or_default();
    let to = parsed.iter().find(|(k, _)| k == "to").map(|(_, v)| v.clone()).unwrap_or_default();
    let subject = parsed.iter().find(|(k, _)| k == "subject").map(|(_, v)| v.clone()).unwrap_or_default();
    let msg_body = parsed.iter().find(|(k, _)| k == "body").map(|(_, v)| v.clone()).unwrap_or_default();

    if from.is_empty() || to.is_empty() {
        return http_response(400, "application/json", br#"{"error":"from and to are required"}"#.to_vec());
    }

    let result = rpc_call(
        mailbox_id.to_string(),
        String::from("theater:inbox/mailbox.put-message"),
        Value::Tuple(alloc::vec![
            Value::String(from),
            Value::String(to),
            Value::String(subject),
            Value::String(msg_body),
        ]),
        Value::Tuple(alloc::vec![]),
    );

    let id = match unwrap_rpc_result(result) {
        Some(Value::U64(n)) => n,
        _ => return http_response(500, "application/json", br#"{"error":"mailbox rpc failed"}"#.to_vec()),
    };

    let json = format!(r#"{{"id":{}}}"#, id);
    http_response(201, "application/json", json.into_bytes())
}

/// `POST /v1/mailboxes/<addr>/send` — deliver a message via SMTP. The
/// sender (`from`) is the address in the URL path; the body provides
/// `to`, optional `cc` + `bcc`, `subject`, `body`, and an optional
/// `smtp_server` fallback. `to`, `cc`, `bcc` may each be a JSON array
/// of addresses; `to` also accepts a bare string for back-compat.
///
/// Recipients are routed per-domain: each is resolved via the built-in
/// domain map (see `resolve_smtp_server`); recipients sharing a server
/// are batched into one SMTP transaction. `smtp_server` from the body
/// is the fallback only for domains the map doesn't know. The To and
/// Cc headers in each transaction show the *full* lists (so recipients
/// see who else got the message); Bcc is never written.
///
/// Per-domain transactions mean partial success is observable. Response
/// shape: `{status, delivered:[...], failed:[{recipient,error}]}`.
/// HTTP 200 if at least one address was delivered (status `sent` or
/// `partial`); 502 if all failed.
///
/// Sender-copy is *not* implicitly recorded. If the sender wants the
/// message in their own mailbox they should Bcc themselves, which goes
/// through the same SMTP code path as any external recipient — including
/// loopback when the address is on this server's domain.
fn handle_send(request_str: &str, from: &str, dkim_private_key_pem: &str) -> Vec<u8> {
    let body = match request_str.find("\r\n\r\n") {
        Some(i) => &request_str[i + 4..],
        None => return http_response(400, "application/json", br#"{"error":"missing body"}"#.to_vec()),
    };

    let parsed = parse_simple_json_object(body);
    let get = |k: &str| parsed.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let to_list = parse_string_or_array(body, "to");
    let cc_list = parse_string_or_array(body, "cc");
    let bcc_list = parse_string_or_array(body, "bcc");
    let subject = get("subject");
    let msg_body = get("body");
    let fallback_server = {
        let s = get("smtp_server");
        if s.is_empty() { None } else { Some(s) }
    };

    if to_list.is_empty() {
        return http_response(400, "application/json", br#"{"error":"to is required"}"#.to_vec());
    }

    // Group every recipient by the SMTP server that should handle it.
    // Unresolvable recipients (no domain match, no fallback) go straight
    // into the `failed` list — we still attempt the others.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut delivered: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for rcpt in to_list.iter().chain(cc_list.iter()).chain(bcc_list.iter()) {
        match resolve_smtp_server(rcpt, fallback_server.as_deref()) {
            Some(server) => match groups.iter_mut().find(|(s, _)| s == &server) {
                Some((_, list)) => list.push(rcpt.clone()),
                None => groups.push((server, alloc::vec![rcpt.clone()])),
            },
            None => failed.push((
                rcpt.clone(),
                format!("no smtp server for domain (and no smtp_server fallback)"),
            )),
        }
    }

    for (server, group_rcpts) in &groups {
        match smtp_deliver(
            server,
            from,
            group_rcpts,
            &to_list,
            &cc_list,
            &subject,
            &msg_body,
            dkim_private_key_pem,
        ) {
            Ok(()) => delivered.extend(group_rcpts.iter().cloned()),
            Err(e) => {
                log(format!(
                    "[inbox-api] smtp deliver to {} failed: {}",
                    server, e
                ));
                for r in group_rcpts {
                    failed.push((r.clone(), e.clone()));
                }
            }
        }
    }

    let status = if failed.is_empty() {
        "sent"
    } else if delivered.is_empty() {
        "failed"
    } else {
        "partial"
    };

    let delivered_json = json_array_of_strings(&delivered);
    let failed_json = {
        let parts: Vec<String> = failed
            .iter()
            .map(|(r, e)| {
                format!(
                    r#"{{"recipient":"{}","error":"{}"}}"#,
                    json_escape(r),
                    json_escape(e)
                )
            })
            .collect();
        format!("[{}]", parts.join(","))
    };
    let body = format!(
        r#"{{"status":"{}","delivered":{},"failed":{}}}"#,
        status, delivered_json, failed_json
    );
    let http_status = if delivered.is_empty() { 502 } else { 200 };
    http_response(http_status, "application/json", body.into_bytes())
}

/// Map a recipient address to the SMTP server we should deliver to.
/// Known domains are hardcoded; unknown domains fall through to the
/// caller-supplied fallback (the body's `smtp_server` field). Real MX
/// lookup is a separate follow-up.
fn resolve_smtp_server(addr: &str, fallback: Option<&str>) -> Option<String> {
    let domain = addr.rsplit('@').next().unwrap_or("");
    match domain {
        "colinrozzi.com" => Some(String::from("localhost:25")),
        "gmail.com" => Some(String::from("gmail-smtp-in.l.google.com:25")),
        _ => fallback.map(String::from),
    }
}

fn json_array_of_strings(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| format!(r#""{}""#, json_escape(s)))
        .collect();
    format!("[{}]", parts.join(","))
}

// ============================================================================
// SMTP client
// ============================================================================

/// Talk SMTP to `server_addr` and deliver one message to `rcpt_to`.
/// `header_to`/`header_cc` are the *full* recipient lists for the
/// outgoing visible headers — they're the same for every per-domain
/// transaction so a message split across servers presents identical
/// To/Cc headers to all recipients.
fn smtp_deliver(
    server_addr: &str,
    from: &str,
    rcpt_to: &[String],
    header_to: &[String],
    header_cc: &[String],
    subject: &str,
    body: &str,
    dkim_private_key_pem: &str,
) -> Result<(), String> {
    let conn = tcp_connect(server_addr.to_string())
        .map_err(|e| format!("connect to {} failed: {}", server_addr, e))?;

    // Greeting (220).
    smtp_expect(&conn, 220).map_err(|e| {
        let _ = tcp_close(conn.clone());
        e
    })?;

    let result = smtp_session(
        &conn,
        server_addr,
        from,
        rcpt_to,
        header_to,
        header_cc,
        subject,
        body,
        dkim_private_key_pem,
    );
    let _ = tcp_close(conn);
    result
}

fn smtp_session(
    conn: &str,
    server_addr: &str,
    from: &str,
    rcpt_to: &[String],
    header_to: &[String],
    header_cc: &[String],
    subject: &str,
    body: &str,
    dkim_private_key_pem: &str,
) -> Result<(), String> {
    let ehlo_resp = smtp_command_get_body(conn, "EHLO inbox.local\r\n", 250)?;

    // Opportunistic STARTTLS: if the server advertises it, upgrade the
    // connection before exchanging mail. RFC 3207 requires re-issuing
    // EHLO over the encrypted channel.
    if smtp_caps_have(&ehlo_resp, "STARTTLS") {
        let server_name = server_addr.split(':').next().unwrap_or(server_addr).to_string();
        smtp_command(conn, "STARTTLS\r\n", 220)?;
        tcp_upgrade_to_tls_client(conn.to_string(), server_name.clone())
            .map_err(|e| format!("starttls upgrade to {}: {}", server_name, e))?;
        log(format!("[inbox-api] STARTTLS upgrade ok to {}", server_name));
        smtp_command(conn, "EHLO inbox.local\r\n", 250)?;
    }

    smtp_command(conn, &format!("MAIL FROM:<{}>\r\n", from), 250)?;
    for rcpt in rcpt_to {
        smtp_command(conn, &format!("RCPT TO:<{}>\r\n", rcpt), 250)?;
    }
    smtp_command(conn, "DATA\r\n", 354)?;

    // Build the headers (without DKIM-Signature yet) + body. DKIM signs the
    // resulting RFC822 message; the signature header gets prepended.
    let now_ms = timer_now();
    let from_local = from.split('@').next().unwrap_or("inbox");
    let message_id = format!("{}.{}@{}", now_ms, from_local, dkim::DOMAIN);
    let mut headers = String::new();
    headers.push_str(&format!("From: {}\r\n", from));
    headers.push_str(&format!("To: {}\r\n", header_to.join(", ")));
    if !header_cc.is_empty() {
        headers.push_str(&format!("Cc: {}\r\n", header_cc.join(", ")));
    }
    headers.push_str(&format!("Subject: {}\r\n", subject));
    headers.push_str(&format!("Date: {}\r\n", rfc2822::format_date(now_ms)));
    headers.push_str(&format!("Message-ID: <{}>\r\n", message_id));
    headers.push_str("MIME-Version: 1.0\r\n");
    headers.push_str("Content-Type: text/plain; charset=utf-8\r\n");

    // Normalize body to CRLF line endings — DKIM canonicalization assumes that.
    let mut body_crlf = String::new();
    for line in body.split('\n') {
        body_crlf.push_str(line.trim_end_matches('\r'));
        body_crlf.push_str("\r\n");
    }

    let signed_headers: &[&str] = if header_cc.is_empty() {
        &["from", "to", "subject", "date", "message-id"]
    } else {
        &["from", "to", "cc", "subject", "date", "message-id"]
    };

    let dkim_signature = dkim::sign_message(
        dkim_private_key_pem,
        dkim::SELECTOR,
        dkim::DOMAIN,
        signed_headers,
        &headers,
        body_crlf.as_bytes(),
    )?;

    let mut data = String::new();
    data.push_str(&dkim_signature);
    data.push_str(&headers);
    data.push_str("\r\n");
    // Dot-stuff: any line starting with "." gets prefixed with another ".".
    for line in body_crlf.split("\r\n") {
        if line.starts_with('.') {
            data.push('.');
        }
        data.push_str(line);
        data.push_str("\r\n");
    }
    // body_crlf ends with "\r\n" so the loop above already emits a trailing
    // blank line; one more for the "." terminator below.
    // Trim the duplicated trailing CRLF that the empty split-produced item caused:
    if data.ends_with("\r\n\r\n\r\n") {
        data.truncate(data.len() - 2);
    }
    data.push_str(".\r\n");

    tcp_send(conn.to_string(), data.into_bytes())
        .map_err(|e| format!("DATA send failed: {}", e))?;
    smtp_expect(conn, 250)?;

    smtp_command(conn, "QUIT\r\n", 221)?;
    Ok(())
}

fn smtp_command(conn: &str, line: &str, expected: u16) -> Result<(), String> {
    smtp_command_get_body(conn, line, expected).map(|_| ())
}

/// Like `smtp_command` but returns the full server response text. EHLO
/// uses this to scan capabilities (STARTTLS, etc.).
fn smtp_command_get_body(conn: &str, line: &str, expected: u16) -> Result<String, String> {
    tcp_send(conn.to_string(), line.as_bytes().to_vec())
        .map_err(|e| format!("send {:?} failed: {}", line.trim(), e))?;
    smtp_expect(conn, expected)
}

/// Read until we see a complete SMTP reply, then assert the code matches.
/// SMTP replies are one or more lines: "NNN-..." for continuation,
/// "NNN ..." (space) for the final line. Returns the full response text
/// so callers like EHLO can scan it for capability advertisements.
fn smtp_expect(conn: &str, expected: u16) -> Result<String, String> {
    let mut buf = Vec::new();
    loop {
        let chunk = tcp_receive(conn.to_string(), 4096)
            .map_err(|e| format!("receive failed: {}", e))?;
        if chunk.is_empty() {
            return Err(String::from("connection closed before reply complete"));
        }
        buf.extend_from_slice(&chunk);
        // Check whether we have a final line (a CRLF preceded by "NNN ").
        if let Some(last_line_start) = find_last_smtp_line_start(&buf) {
            if buf.len() > last_line_start + 3 && buf[last_line_start + 3] == b' '
                && buf.ends_with(b"\r\n")
            {
                let code = parse_smtp_code(&buf[last_line_start..]).ok_or_else(|| {
                    String::from("invalid SMTP reply (no 3-digit code on final line)")
                })?;
                let text = core::str::from_utf8(&buf)
                    .map_err(|_| String::from("non-utf8 SMTP reply"))?
                    .to_string();
                if code != expected {
                    return Err(format!("expected {}, got {}: {}", expected, code, text.trim()));
                }
                return Ok(text);
            }
        }
    }
}

/// True if the EHLO multi-line response advertises `cap`. Tolerant of
/// whitespace; matches the capability token in `250-CAP ...` or `250 CAP`.
fn smtp_caps_have(resp: &str, cap: &str) -> bool {
    for line in resp.split("\r\n") {
        let trimmed = line.trim();
        // Lines look like "250-FOO" or "250 FOO ..." — strip "250-" or "250 "
        let payload = if trimmed.len() >= 4 {
            &trimmed[4..]
        } else {
            continue;
        };
        let token = payload.split_whitespace().next().unwrap_or("");
        if token.eq_ignore_ascii_case(cap) {
            return true;
        }
    }
    false
}

/// Find the start index of the last line in `buf`. A "line" ends with CRLF.
fn find_last_smtp_line_start(buf: &[u8]) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    // Find the last CRLF and take what follows; if the buffer ends with CRLF,
    // skip it and look for the previous one.
    let end = if buf.ends_with(b"\r\n") { buf.len() - 2 } else { buf.len() };
    let slice = &buf[..end];
    match slice.windows(2).rposition(|w| w == b"\r\n") {
        Some(pos) => Some(pos + 2),
        None => Some(0),
    }
}

fn parse_smtp_code(line: &[u8]) -> Option<u16> {
    if line.len() < 3 {
        return None;
    }
    let s = core::str::from_utf8(&line[..3]).ok()?;
    s.parse::<u16>().ok()
}

// ============================================================================
// Helpers
// ============================================================================

/// The RPC handler returns `Result<Variant<"ok" | "err", [response]>, ...>`:
/// the outer Result is "did the RPC reach the actor", and the Variant
/// represents the actor's own `Result<T, E>` return. The runtime already
/// strips the state half of the actor's `(state, response)` return, so
/// the variant payload is just the response value.
fn unwrap_rpc_result(value: Value) -> Option<Value> {
    match value {
        Value::Result { value: Ok(inner), .. } => match *inner {
            Value::Variant {
                case_name, payload, ..
            } if case_name == "ok" => payload.into_iter().next(),
            _ => None,
        },
        _ => None,
    }
}

/// Extract the bearer token from `Authorization: Bearer <token>`. Header
/// name match is case-insensitive; the "Bearer " prefix is case-insensitive;
/// the token itself is verbatim.
fn extract_bearer(request_str: &str) -> Option<&str> {
    let headers_end = request_str.find("\r\n\r\n")?;
    let headers = &request_str[..headers_end];
    for line in headers.split("\r\n") {
        let Some(colon) = line.find(':') else { continue }; // request-line has no colon
        if !line[..colon].eq_ignore_ascii_case("authorization") {
            continue;
        }
        let value = line[colon + 1..].trim_start();
        let lower = value.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("bearer ") {
            let prefix_len = value.len() - rest.len();
            return Some(value[prefix_len..].trim());
        }
    }
    None
}

fn http_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status, status_text, content_type, body.len()
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(&body);
    out
}

fn inbox_page_to_json(page: &Value) -> String {
    // page is a Record<inbox-page>{ messages: list<message>, next_cursor: u64 }
    let (messages, next_cursor) = match page {
        Value::Record { fields, .. } => {
            let messages = fields.iter().find(|(k, _)| k == "messages").map(|(_, v)| v).cloned();
            let next_cursor = fields.iter().find(|(k, _)| k == "next-cursor" || k == "next_cursor")
                .and_then(|(_, v)| if let Value::U64(n) = v { Some(*n) } else { None })
                .unwrap_or(0);
            (messages, next_cursor)
        }
        _ => (None, 0),
    };

    let messages_json = match messages {
        Some(Value::List { items, .. }) => {
            let parts: Vec<String> = items.iter().map(message_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        _ => String::from("[]"),
    };

    format!(r#"{{"messages":{},"next_cursor":{}}}"#, messages_json, next_cursor)
}

fn message_to_json(msg: &Value) -> String {
    let mut id = 0u64;
    let mut from = String::new();
    let mut to = String::new();
    let mut subject = String::new();
    let mut body = String::new();
    let mut received_at = 0u64;
    if let Value::Record { fields, .. } = msg {
        for (k, v) in fields {
            match (k.as_str(), v) {
                ("id", Value::U64(n)) => id = *n,
                ("from", Value::String(s)) => from = s.clone(),
                ("to", Value::String(s)) => to = s.clone(),
                ("subject", Value::String(s)) => subject = s.clone(),
                ("body", Value::String(s)) => body = s.clone(),
                ("received-at", Value::U64(n)) | ("received_at", Value::U64(n)) => received_at = *n,
                _ => {}
            }
        }
    }
    format!(
        r#"{{"id":{},"from":"{}","to":"{}","subject":"{}","body":"{}","received_at":{}}}"#,
        id, json_escape(&from), json_escape(&to), json_escape(&subject), json_escape(&body), received_at
    )
}

/// Minimal URL percent-decoder. Plus signs become spaces (`form-urlencoded`
/// convention), `%XX` decodes one hex byte. Sufficient for our query params
/// and path segments — we do not need full RFC 3986 fidelity.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = core::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Find `"<key>":` in `s` and parse its value as a list of strings.
/// Accepts either a bare string (returns a single-element vec) or a
/// JSON array of strings. Returns an empty vec if the key is missing
/// or the value is not a recognized shape. Companion to
/// `parse_simple_json_object`, which only handles flat string values.
fn parse_string_or_array(s: &str, key: &str) -> Vec<String> {
    let needle = format!(r#""{}":"#, key);
    let Some(i) = s.find(&needle) else { return Vec::new() };
    let after = s[i + needle.len()..].trim_start();
    let bytes = after.as_bytes();
    if bytes.is_empty() {
        return Vec::new();
    }
    match bytes[0] {
        b'"' => {
            let mut item = Vec::new();
            let mut j = 1;
            while j < bytes.len() && bytes[j] != b'"' {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    let esc = match bytes[j + 1] {
                        b'"' => b'"',
                        b'\\' => b'\\',
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        other => other,
                    };
                    item.push(esc);
                    j += 2;
                } else {
                    item.push(bytes[j]);
                    j += 1;
                }
            }
            let s = String::from_utf8(item).unwrap_or_default();
            if s.is_empty() { Vec::new() } else { alloc::vec![s] }
        }
        b'[' => {
            let mut out: Vec<String> = Vec::new();
            let mut j = 1;
            loop {
                while j < bytes.len()
                    && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r' | b',')
                {
                    j += 1;
                }
                if j >= bytes.len() || bytes[j] == b']' {
                    return out;
                }
                if bytes[j] != b'"' {
                    return out;
                }
                j += 1;
                let mut item = Vec::new();
                while j < bytes.len() && bytes[j] != b'"' {
                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                        let esc = match bytes[j + 1] {
                            b'"' => b'"',
                            b'\\' => b'\\',
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            other => other,
                        };
                        item.push(esc);
                        j += 2;
                    } else {
                        item.push(bytes[j]);
                        j += 1;
                    }
                }
                if j < bytes.len() {
                    j += 1;
                }
                if let Ok(s) = String::from_utf8(item) {
                    if !s.is_empty() {
                        out.push(s);
                    }
                }
            }
        }
        _ => Vec::new(),
    }
}

/// Minimal JSON parser: flat object with string values only.
/// Returns Vec<(key, value)> in source order. Unrecognized syntax → empty Vec.
fn parse_simple_json_object(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let s = s.trim();
    let s = match s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        Some(inner) => inner,
        None => return out,
    };
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // skip whitespace and commas
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        // expect "key"
        if chars[i] != '"' {
            return out;
        }
        i += 1;
        let key_start = i;
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' {
                i += 1;
            }
            i += 1;
        }
        let key: String = chars[key_start..i].iter().collect();
        i += 1; // skip closing "
                // skip whitespace then ":"
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != ':' {
            return out;
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            return out;
        }
        // Skip non-string values (e.g. JSON arrays) — those are parsed
        // out of band by helpers like `parse_string_or_array`. We still
        // want to keep parsing later keys.
        if chars[i] == '[' {
            let mut depth = 1;
            i += 1;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '[' => {
                        depth += 1;
                        i += 1;
                    }
                    ']' => {
                        depth -= 1;
                        i += 1;
                    }
                    '"' => {
                        i += 1;
                        while i < chars.len() && chars[i] != '"' {
                            if chars[i] == '\\' && i + 1 < chars.len() {
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        if i < chars.len() {
                            i += 1;
                        }
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        if chars[i] != '"' {
            return out;
        }
        i += 1;
        let mut value = String::new();
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' && i + 1 < chars.len() {
                let esc = chars[i + 1];
                match esc {
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    c => value.push(c),
                }
                i += 2;
            } else {
                value.push(chars[i]);
                i += 1;
            }
        }
        i += 1; // skip closing "
        out.push((key, value));
    }
    out
}
