//! Inbox API handler: per-connection HTTP handler.
//!
//! Receives one HTTP request, routes it, calls into the mailbox actor via
//! RPC, returns JSON, closes the connection, and shuts itself down.
//!
//! Routes:
//!   GET  /v1/inbox?since=<n>   → list messages since cursor n
//!   POST /v1/messages          → store a message; body is JSON
//!     { "from": "...", "to": "...", "subject": "...", "body": "..." }

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct HandlerState {
    pub mailbox_id: String,
}

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
            shutdown: func(data: option<list<u8>>) -> result<_, string>,
        }
        theater:simple/tcp {
            receive: func(connection-id: string, max-bytes: u32) -> result<list<u8>, string>,
            send: func(connection-id: string, data: list<u8>) -> result<u64, string>,
            close: func(connection-id: string) -> result<_, string>,
        }
        theater:simple/rpc {
            call: func(actor-id: string, function: string, params: value, options: value) -> value,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value, mailbox-id: string) -> result<handler-state, string>,
        theater:simple/tcp-client.handle-connection-transfer: func(state: handler-state, connection-id: string) -> result<handler-state, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/runtime", name = "shutdown")]
fn shutdown(data: Option<Vec<u8>>) -> Result<(), String>;

#[import(module = "theater:simple/tcp", name = "receive")]
fn tcp_receive(connection_id: String, max_bytes: u32) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/tcp", name = "send")]
fn tcp_send(connection_id: String, data: Vec<u8>) -> Result<u64, String>;

#[import(module = "theater:simple/tcp", name = "close")]
fn tcp_close(connection_id: String) -> Result<(), String>;

#[import(module = "theater:simple/rpc", name = "call")]
fn rpc_call(actor_id: String, function: String, params: Value, options: Value) -> Value;

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value, mailbox_id: String) -> Result<(HandlerState, ()), String> {
    Ok((HandlerState { mailbox_id }, ()))
}

#[export(name = "theater:simple/tcp-client.handle-connection-transfer")]
fn handle_connection_transfer(
    state: HandlerState,
    connection_id: String,
) -> Result<(HandlerState, ()), String> {
    let request = tcp_receive(connection_id.clone(), 65536).unwrap_or_default();

    let response = route(&request, &state.mailbox_id);

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

fn route(request: &[u8], mailbox_id: &str) -> Vec<u8> {
    let request_str = match core::str::from_utf8(request) {
        Ok(s) => s,
        Err(_) => return http_response(400, "application/json", br#"{"error":"non-utf8 request"}"#.to_vec()),
    };

    let first_line = request_str.lines().next().unwrap_or("");
    let mut parts = first_line.split(' ');
    let method = parts.next().unwrap_or("");
    let path_and_query = parts.next().unwrap_or("/");
    let (path, query) = match path_and_query.find('?') {
        Some(i) => (&path_and_query[..i], &path_and_query[i + 1..]),
        None => (path_and_query, ""),
    };

    match (method, path) {
        ("GET", "/v1/inbox") => handle_inbox(query, mailbox_id),
        ("POST", "/v1/messages") => handle_post_message(request_str, mailbox_id),
        _ => http_response(404, "application/json", br#"{"error":"not found"}"#.to_vec()),
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

fn http_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
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
        if i >= chars.len() || chars[i] != '"' {
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
