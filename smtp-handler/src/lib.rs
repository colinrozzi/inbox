//! SMTP handler: drives one inbound SMTP session.
//!
//! Server-side state machine: reads commands, parses headers/body on DATA,
//! and on a complete message RPCs into the mailbox to store it.
//! Adapted from /home/colin/work/actors/mail/smtp-handler.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct SmtpHandlerState {
    pub router_id: String,
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
        theater:simple/actor.init: func(state: value, router-id: string) -> result<smtp-handler-state, string>,
        theater:simple/tcp-client.handle-connection-transfer: func(state: smtp-handler-state, connection-id: string) -> result<smtp-handler-state, string>,
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

const HOSTNAME: &str = "inbox.local";

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value, router_id: String) -> Result<(SmtpHandlerState, ()), String> {
    Ok((SmtpHandlerState { router_id }, ()))
}

#[export(name = "theater:simple/tcp-client.handle-connection-transfer")]
fn handle_connection_transfer(
    state: SmtpHandlerState,
    connection_id: String,
) -> Result<(SmtpHandlerState, ()), String> {
    if let Err(e) = run_session(&connection_id, &state.router_id) {
        log(format!("[inbox-smtp] session error: {}", e));
    }
    let _ = tcp_close(connection_id);
    let _ = shutdown(None);
    Ok((state, ()))
}

// ============================================================================
// SMTP server state machine
// ============================================================================

fn run_session(conn: &str, router_id: &str) -> Result<(), String> {
    send_line(conn, &format!("220 {} ESMTP inbox", HOSTNAME))?;

    let mut mail_from: Option<String> = None;
    // Each accepted recipient: (address, mailbox_id) resolved via the router.
    let mut rcpts: Vec<(String, String)> = Vec::new();

    loop {
        let line = read_line(conn)?;
        let trimmed = line.trim();
        let cmd = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_uppercase();

        match cmd.as_str() {
            "EHLO" | "HELO" => {
                let domain = trimmed.split_whitespace().nth(1).unwrap_or("anonymous");
                send_line(conn, &format!("250-{} hello {}", HOSTNAME, domain))?;
                send_line(conn, "250-SIZE 10485760")?;
                send_line(conn, "250 OK")?;
                mail_from = None;
                rcpts.clear();
            }
            "MAIL" => match extract_address(trimmed, "FROM:") {
                Some(addr) => {
                    mail_from = Some(addr);
                    send_line(conn, "250 OK")?;
                }
                None => send_line(conn, "501 Syntax error in MAIL FROM")?,
            },
            "RCPT" => {
                if mail_from.is_none() {
                    send_line(conn, "503 Need MAIL command first")?;
                } else {
                    match extract_address(trimmed, "TO:") {
                        Some(addr) => match router_lookup(router_id, &addr) {
                            Ok(Some(mbox_id)) => {
                                rcpts.push((addr, mbox_id));
                                send_line(conn, "250 OK")?;
                            }
                            Ok(None) => {
                                send_line(conn, "550 No such recipient")?;
                            }
                            Err(e) => {
                                log(format!("[inbox-smtp] router lookup failed: {}", e));
                                send_line(conn, "451 Temporary lookup failure")?;
                            }
                        },
                        None => send_line(conn, "501 Syntax error in RCPT TO")?,
                    }
                }
            }
            "DATA" => {
                if mail_from.is_none() {
                    send_line(conn, "503 Need MAIL command first")?;
                    continue;
                }
                if rcpts.is_empty() {
                    send_line(conn, "503 Need RCPT command first")?;
                    continue;
                }
                send_line(conn, "354 Start mail input; end with <CRLF>.<CRLF>")?;

                let raw = read_data_block(conn)?;
                let (subject, body) = parse_headers_and_body(&raw);
                let from = mail_from.clone().unwrap_or_default();

                // Store on each recipient's mailbox.
                for (to, mbox_id) in &rcpts {
                    let _ = rpc_call(
                        mbox_id.clone(),
                        String::from("theater:inbox/mailbox.put-message"),
                        Value::Tuple(alloc::vec![
                            Value::String(from.clone()),
                            Value::String(to.clone()),
                            Value::String(subject.clone()),
                            Value::String(body.clone()),
                        ]),
                        Value::Tuple(alloc::vec![]),
                    );
                }
                log(format!(
                    "[inbox-smtp] stored {} byte message from {} to {} recipients",
                    raw.len(),
                    from,
                    rcpts.len()
                ));

                send_line(conn, "250 OK message queued")?;
                mail_from = None;
                rcpts.clear();
            }
            "RSET" => {
                mail_from = None;
                rcpts.clear();
                send_line(conn, "250 OK")?;
            }
            "NOOP" => {
                send_line(conn, "250 OK")?;
            }
            "QUIT" => {
                send_line(conn, &format!("221 {} closing", HOSTNAME))?;
                return Ok(());
            }
            "" => {} // ignore blank lines
            _ => send_line(conn, "502 Command not implemented")?,
        }
    }
}

/// Ask the router to resolve an address to a mailbox actor ID.
fn router_lookup(router_id: &str, address: &str) -> Result<Option<String>, String> {
    let result = rpc_call(
        router_id.to_string(),
        String::from("theater:inbox/router.lookup"),
        Value::Tuple(alloc::vec![Value::String(address.to_string())]),
        Value::Tuple(alloc::vec![]),
    );
    // Result<Variant<"ok", [Option<String>]>, _>
    let ok_payload = match result {
        Value::Result { value: Ok(inner), .. } => match *inner {
            Value::Variant { case_name, payload, .. } if case_name == "ok" => {
                payload.into_iter().next()
            }
            _ => None,
        },
        _ => None,
    };
    match ok_payload {
        Some(Value::Option { value: Some(inner), .. }) => match *inner {
            Value::String(id) => Ok(Some(id)),
            _ => Err(String::from("unexpected router response shape")),
        },
        Some(Value::Option { value: None, .. }) => Ok(None),
        _ => Err(String::from("router rpc returned no payload")),
    }
}

fn extract_address(line: &str, marker: &str) -> Option<String> {
    let upper = line.to_uppercase();
    let pos = upper.find(marker)?;
    let rest = line[pos + marker.len()..].trim();
    // Address may be wrapped in <...>.
    let inner = if let (Some(start), Some(end)) = (rest.find('<'), rest.find('>')) {
        if start < end {
            &rest[start + 1..end]
        } else {
            rest
        }
    } else {
        rest.split_whitespace().next().unwrap_or(rest)
    };
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

/// Read a DATA block: lines until a single "." on its own line.
/// Strips dot-stuffing (lines starting with ".." → ".").
fn read_data_block(conn: &str) -> Result<String, String> {
    let mut out = String::new();
    loop {
        let line = read_line(conn)?;
        // Trim only the trailing CRLF, keep internal whitespace.
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if line == "." {
            return Ok(out);
        }
        let line = if let Some(stripped) = line.strip_prefix("..") {
            // Dot-stuffing: ".." → "."
            let mut s = String::from(".");
            s.push_str(stripped);
            s
        } else {
            line.to_string()
        };
        out.push_str(&line);
        out.push('\n');
    }
}

/// Pull `Subject:` out and return (subject, body). Body starts after the
/// blank line that ends the header section.
fn parse_headers_and_body(raw: &str) -> (String, String) {
    let mut subject = String::new();
    let mut header_end = 0usize;
    let mut in_headers = true;
    for line in raw.split_inclusive('\n') {
        if in_headers {
            let stripped = line.trim_end();
            if stripped.is_empty() {
                in_headers = false;
                header_end += line.len();
                continue;
            }
            if let Some(rest) = stripped.strip_prefix("Subject:")
                .or_else(|| stripped.strip_prefix("subject:"))
                .or_else(|| stripped.strip_prefix("SUBJECT:"))
            {
                subject = rest.trim().to_string();
            }
            header_end += line.len();
        } else {
            break;
        }
    }
    let body = raw.get(header_end..).unwrap_or("").trim_end().to_string();
    (subject, body)
}

// ============================================================================
// Wire helpers
// ============================================================================

fn send_line(conn: &str, line: &str) -> Result<(), String> {
    let mut data = line.as_bytes().to_vec();
    data.extend_from_slice(b"\r\n");
    tcp_send(conn.to_string(), data)
        .map(|_| ())
        .map_err(|e| format!("send failed: {}", e))
}

fn read_line(conn: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    loop {
        let chunk = tcp_receive(conn.to_string(), 1)
            .map_err(|e| format!("receive failed: {}", e))?;
        if chunk.is_empty() {
            return Err(String::from("connection closed"));
        }
        buf.extend_from_slice(&chunk);
        if buf.ends_with(b"\n") {
            break;
        }
        if buf.len() > 4096 {
            return Err(String::from("line too long"));
        }
    }
    core::str::from_utf8(&buf)
        .map(|s| s.to_string())
        .map_err(|_| String::from("invalid utf-8 in SMTP line"))
}
