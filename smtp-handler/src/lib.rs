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
        theater:simple/actor.init: func(state: value) -> result<smtp-handler-state, string>,
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
fn init(state: Value) -> Result<(SmtpHandlerState, ()), String> {
    let router_id = match state {
        Value::String(s) => s,
        _ => return Err(String::from(
            "smtp-handler init: expected init_state = string (router actor id)",
        )),
    };
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
                let parsed = parse_headers_and_body(&raw);
                let from = mail_from.clone().unwrap_or_default();

                // Store on each recipient's mailbox. Every copy carries the TRUE
                // envelope To/Cc (from the DATA headers), NOT the recipient's own
                // RCPT-TO leg (`_leg`) — so a cc-d reader sees the real recipient
                // set and knows they were cc-d, not mis-addressed. Fall back to the
                // leg only if the message carried no To: header (e.g. bcc-only), so
                // `to` is never blank.
                let header_to = if parsed.to.trim().is_empty() {
                    None
                } else {
                    Some(parsed.to.clone())
                };
                for (leg, mbox_id) in &rcpts {
                    let to = header_to.clone().unwrap_or_else(|| leg.clone());
                    let _ = rpc_call(
                        mbox_id.clone(),
                        String::from("theater:inbox/mailbox.put-message"),
                        Value::Tuple(alloc::vec![
                            Value::String(from.clone()),
                            Value::String(to),
                            Value::String(parsed.subject.clone()),
                            Value::String(parsed.body.clone()),
                            Value::String(parsed.message_id.clone()),
                            Value::String(parsed.in_reply_to.clone()),
                            Value::String(parsed.references.clone()),
                            Value::String(parsed.cc.clone()),
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

/// A parsed inbound message: the display fields plus the RFC 5322 threading
/// headers (`Message-ID` / `In-Reply-To` / `References`) so a reply can chain
/// back into the same conversation. Threading headers are empty when the
/// sender didn't set them.
struct ParsedMessage {
    /// The message's true `To` / `Cc` headers (verbatim, comma-joined multi-
    /// recipient lists) — the real envelope, so each stored per-mailbox copy can
    /// show the full recipient set rather than that mailbox's own delivery leg.
    to: String,
    cc: String,
    subject: String,
    body: String,
    message_id: String,
    in_reply_to: String,
    references: String,
}

/// Parse the RFC 822 message: pull out `Subject`, the threading headers, find
/// the content-type, and extract a clean body. For `multipart/*` messages the
/// first `text/plain` part wins; for plain messages the whole post-header
/// section is the body. Handles `quoted-printable` and `base64`
/// Content-Transfer-Encoding.
fn parse_headers_and_body(raw: &str) -> ParsedMessage {
    let mut header_end = 0usize;
    let mut to = String::new();
    let mut cc = String::new();
    let mut subject = String::new();
    let mut content_type = String::new();
    let mut content_encoding = String::new();
    let mut message_id = String::new();
    let mut in_reply_to = String::new();
    let mut references = String::new();
    let mut last_header_name: Option<String> = None;

    for line in raw.split_inclusive('\n') {
        let stripped = line.trim_end_matches(|c| c == '\n' || c == '\r');
        if stripped.is_empty() {
            header_end += line.len();
            break;
        }
        // Header folding: a continuation line starts with WSP and belongs to
        // the previous header's value. `References` in particular is routinely
        // folded across several lines.
        if (line.starts_with(' ') || line.starts_with('\t')) && last_header_name.is_some() {
            match last_header_name.as_deref() {
                Some(n) if n.eq_ignore_ascii_case("to") => {
                    to.push(' ');
                    to.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("cc") => {
                    cc.push(' ');
                    cc.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("subject") => {
                    subject.push(' ');
                    subject.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("content-type") => {
                    content_type.push(' ');
                    content_type.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("content-transfer-encoding") => {
                    content_encoding.push(' ');
                    content_encoding.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("message-id") => {
                    message_id.push(' ');
                    message_id.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("in-reply-to") => {
                    in_reply_to.push(' ');
                    in_reply_to.push_str(stripped.trim());
                }
                Some(n) if n.eq_ignore_ascii_case("references") => {
                    references.push(' ');
                    references.push_str(stripped.trim());
                }
                _ => {}
            }
            header_end += line.len();
            continue;
        }
        if let Some(colon) = stripped.find(':') {
            let name = stripped[..colon].trim().to_string();
            let value = stripped[colon + 1..].trim().to_string();
            if name.eq_ignore_ascii_case("to") {
                to = value;
            } else if name.eq_ignore_ascii_case("cc") {
                cc = value;
            } else if name.eq_ignore_ascii_case("subject") {
                subject = value;
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = value;
            } else if name.eq_ignore_ascii_case("content-transfer-encoding") {
                content_encoding = value;
            } else if name.eq_ignore_ascii_case("message-id") {
                message_id = value;
            } else if name.eq_ignore_ascii_case("in-reply-to") {
                in_reply_to = value;
            } else if name.eq_ignore_ascii_case("references") {
                references = value;
            }
            last_header_name = Some(name);
        }
        header_end += line.len();
    }

    let body_raw = raw.get(header_end..).unwrap_or("");
    let body = if let Some(boundary) = multipart_boundary(&content_type) {
        extract_text_plain(body_raw, &boundary).unwrap_or_else(|| body_raw.trim_end().to_string())
    } else {
        decode_transfer_encoding(body_raw, &content_encoding)
            .trim_end()
            .to_string()
    };
    ParsedMessage {
        to,
        cc,
        subject,
        body,
        message_id,
        in_reply_to,
        references,
    }
}

/// If `Content-Type: multipart/...; boundary=...`, return the boundary value.
fn multipart_boundary(content_type: &str) -> Option<String> {
    let lower = content_type.to_lowercase();
    if !lower.starts_with("multipart/") {
        return None;
    }
    // Find boundary= (case-insensitive). Value may be quoted.
    let needle = "boundary=";
    let idx = lower.find(needle)?;
    let rest = &content_type[idx + needle.len()..];
    let value = if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        stripped[..end].to_string()
    } else {
        rest.split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    };
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Walk parts separated by `--boundary` and return the first `text/plain`
/// part's decoded body.
fn extract_text_plain(body_raw: &str, boundary: &str) -> Option<String> {
    let delim = format!("--{}", boundary);
    let parts: Vec<&str> = body_raw.split(delim.as_str()).collect();
    // First chunk is the preamble (before the first boundary); skip it.
    // Last chunk is the epilogue (after `--boundary--`); skip it.
    for part in parts.iter().skip(1) {
        let part = part.trim_start_matches('\r').trim_start_matches('\n');
        if part.starts_with("--") {
            continue; // closing boundary
        }
        // Each part has its own headers + blank line + body.
        let blank = part.find("\r\n\r\n").or_else(|| part.find("\n\n"));
        if let Some(b) = blank {
            let part_headers = &part[..b];
            let sep_len = if part[b..].starts_with("\r\n\r\n") { 4 } else { 2 };
            let part_body = &part[b + sep_len..];

            let mut ct = String::new();
            let mut cte = String::new();
            for line in part_headers.split('\n') {
                let stripped = line.trim_end_matches(|c| c == '\r' || c == '\n');
                if let Some(colon) = stripped.find(':') {
                    let name = stripped[..colon].trim();
                    let value = stripped[colon + 1..].trim();
                    if name.eq_ignore_ascii_case("content-type") {
                        ct = value.to_string();
                    } else if name.eq_ignore_ascii_case("content-transfer-encoding") {
                        cte = value.to_string();
                    }
                }
            }

            if ct.to_lowercase().starts_with("text/plain") {
                return Some(decode_transfer_encoding(part_body, &cte).trim_end().to_string());
            }
        }
    }
    None
}

/// Decode a body fragment by its Content-Transfer-Encoding.
fn decode_transfer_encoding(body: &str, encoding: &str) -> String {
    match encoding.trim().to_lowercase().as_str() {
        "" | "7bit" | "8bit" | "binary" => body.to_string(),
        "quoted-printable" => decode_quoted_printable(body),
        "base64" => decode_base64_text(body),
        _ => body.to_string(),
    }
}

/// Decode `=XX` hex escapes and soft line breaks (`=\r\n` or `=\n`).
fn decode_quoted_printable(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            // Soft break.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                i += 3;
                continue;
            }
            // =XX hex.
            if i + 2 < bytes.len() {
                let hi = hex_nibble(bytes[i + 1]);
                let lo = hex_nibble(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode a base64 body (stripping CR/LF/WS first).
fn decode_base64_text(input: &str) -> String {
    use base64::engine::{general_purpose::STANDARD as B64, Engine as _};
    let cleaned: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    match B64.decode(cleaned.as_bytes()) {
        Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
        Err(_) => input.to_string(),
    }
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
