//! inbox-cli: a one-shot Theater actor that wraps the inbox HTTP API.
//!
//! The wrapper script (`inbox/cli/inbox`) builds a temporary manifest with
//! `initial_state` set to a JSON document describing the command, ssh's
//! into the deployment host, and runs `theater start`. We parse the
//! command, talk HTTP to localhost:8080, write formatted output to stdout
//! via the terminal handler, and shut down.
//!
//! Intentionally minimal — the point isn't a polished tool, it's to
//! exercise theater as a CLI runtime and see what hurts.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, Value};

packr_guest::setup_guest!();

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
            shutdown: func(data: option<list<u8>>) -> result<_, string>,
        }
        theater:simple/tcp {
            connect: func(address: string) -> result<string, string>,
            send: func(connection-id: string, data: list<u8>) -> result<u64, string>,
            receive: func(connection-id: string, max-bytes: u32) -> result<list<u8>, string>,
            close: func(connection-id: string) -> result<_, string>,
        }
        theater:simple/terminal {
            write-stdout: func(data: list<u8>) -> result<u64, string>,
            write-stderr: func(data: list<u8>) -> result<u64, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<tuple<bool, _>, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/runtime", name = "shutdown")]
fn shutdown(data: Option<Vec<u8>>) -> Result<(), String>;

#[import(module = "theater:simple/tcp", name = "connect")]
fn tcp_connect(address: String) -> Result<String, String>;

#[import(module = "theater:simple/tcp", name = "send")]
fn tcp_send(connection_id: String, data: Vec<u8>) -> Result<u64, String>;

#[import(module = "theater:simple/tcp", name = "receive")]
fn tcp_receive(connection_id: String, max_bytes: u32) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/tcp", name = "close")]
fn tcp_close(connection_id: String) -> Result<(), String>;

#[import(module = "theater:simple/terminal", name = "write-stdout")]
fn write_stdout(data: Vec<u8>) -> Result<u64, String>;

#[import(module = "theater:simple/terminal", name = "write-stderr")]
fn write_stderr(data: Vec<u8>) -> Result<u64, String>;

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(bool, ()), String> {
    let raw = match state {
        Value::String(s) => s,
        _ => {
            err("cli: expected initial_state = JSON string with {cmd, ...}\n");
            shutdown_now();
            return Ok((false, ()));
        }
    };

    let req = match parse_request(&raw) {
        Ok(r) => r,
        Err(e) => {
            err(&format!("cli: parse error: {}\n", e));
            shutdown_now();
            return Ok((false, ()));
        }
    };

    let ok = run(&req).map(|_| true).unwrap_or_else(|e| {
        err(&format!("cli: {}\n", e));
        false
    });
    shutdown_now();
    Ok((ok, ()))
}

fn shutdown_now() {
    let _ = shutdown(None);
}

// ============================================================================
// Request parsing — very small, hand-rolled JSON-ish
// ============================================================================

/// A parsed CLI request — what the wrapper script gives us in initial_state.
struct Request {
    cmd: String,
    api: String,
    token: String,
    // Subset of fields we look up by name; not every cmd uses every field.
    address: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: Option<String>,
    body: Option<String>,
    smtp_server: Option<String>,
    since: Option<u64>,
    full: bool,
}

/// Parse a JSON object like {"cmd":"read","address":"claude@..."}.
/// Only handles the shape we actually produce in the wrapper.
fn parse_request(s: &str) -> Result<Request, String> {
    let mut req = Request {
        cmd: String::new(),
        api: String::from("mail.colinrozzi.com:443"),
        token: String::new(),
        address: None,
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: None,
        body: None,
        smtp_server: None,
        since: None,
        full: false,
    };
    for (k, v) in parse_json_object(s)? {
        match k.as_str() {
            "cmd" => req.cmd = v.as_str().to_string(),
            "api" => req.api = v.as_str().to_string(),
            "token" => req.token = v.as_str().to_string(),
            "address" => req.address = Some(v.as_str().to_string()),
            "to" => req.to = v.as_list(),
            "cc" => req.cc = v.as_list(),
            "bcc" => req.bcc = v.as_list(),
            "subject" => req.subject = Some(v.as_str().to_string()),
            "body" => req.body = Some(v.as_str().to_string()),
            "smtp_server" => req.smtp_server = Some(v.as_str().to_string()),
            "since" => req.since = v.as_str().parse::<u64>().ok(),
            "full" => req.full = v.as_str() == "true",
            _ => {}
        }
    }
    if req.cmd.is_empty() {
        return Err(String::from("missing 'cmd' field"));
    }
    if req.token.is_empty() {
        return Err(String::from("missing 'token' field (set INBOX_TOKEN)"));
    }
    Ok(req)
}

/// Minimal JSON object parser: only handles flat {"str": "str" | number | bool}.
/// Returns (key, value) pairs as strings (numbers/bools coerced to their text).
fn parse_json_object(s: &str) -> Result<Vec<(String, JsonScalar)>, String> {
    let s = s.trim();
    let s = s.strip_prefix('{').ok_or("expected '{'")?;
    let s = s.strip_suffix('}').ok_or("expected '}'")?;
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            break;
        }
        let (key, j) = parse_string(bytes, i)?;
        i = skip_ws(bytes, j);
        if i >= bytes.len() || bytes[i] != b':' {
            return Err(format!("expected ':' at byte {}", i));
        }
        i += 1;
        i = skip_ws(bytes, i);
        let (val, j) = parse_value(bytes, i)?;
        out.push((key, val));
        i = skip_ws(bytes, j);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        }
    }
    Ok(out)
}

enum JsonScalar {
    Str(String),
    List(Vec<String>),
}

impl JsonScalar {
    fn as_str(&self) -> &str {
        match self {
            JsonScalar::Str(s) => s.as_str(),
            JsonScalar::List(_) => "",
        }
    }
    fn as_list(&self) -> Vec<String> {
        match self {
            JsonScalar::Str(s) if !s.is_empty() => alloc::vec![s.clone()],
            JsonScalar::Str(_) => Vec::new(),
            JsonScalar::List(items) => items.clone(),
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn parse_string(b: &[u8], i: usize) -> Result<(String, usize), String> {
    if i >= b.len() || b[i] != b'"' {
        return Err(format!("expected '\"' at byte {}", i));
    }
    let mut out = Vec::new();
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'"' => {
                return Ok((
                    String::from_utf8(out).map_err(|_| String::from("non-utf8 in string"))?,
                    j + 1,
                ));
            }
            b'\\' if j + 1 < b.len() => {
                let escaped = match b[j + 1] {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    other => other,
                };
                out.push(escaped);
                j += 2;
            }
            c => {
                out.push(c);
                j += 1;
            }
        }
    }
    Err(String::from("unterminated string"))
}

fn parse_value(b: &[u8], i: usize) -> Result<(JsonScalar, usize), String> {
    if i >= b.len() {
        return Err(String::from("unexpected end of value"));
    }
    match b[i] {
        b'"' => {
            let (s, j) = parse_string(b, i)?;
            Ok((JsonScalar::Str(s), j))
        }
        c if c.is_ascii_digit() || c == b'-' => {
            let mut j = i;
            while j < b.len()
                && (b[j].is_ascii_digit() || b[j] == b'.' || b[j] == b'-' || b[j] == b'+')
            {
                j += 1;
            }
            let n = core::str::from_utf8(&b[i..j])
                .map_err(|_| String::from("bad number"))?
                .to_string();
            Ok((JsonScalar::Str(n), j))
        }
        b'[' => {
            // Flat array of strings — our only non-scalar value shape.
            let mut items = Vec::new();
            let mut j = i + 1;
            loop {
                j = skip_ws(b, j);
                if j >= b.len() {
                    return Err(String::from("unterminated array"));
                }
                if b[j] == b']' {
                    return Ok((JsonScalar::List(items), j + 1));
                }
                if b[j] != b'"' {
                    return Err(format!("expected '\"' in array at byte {}", j));
                }
                let (s, k) = parse_string(b, j)?;
                items.push(s);
                j = skip_ws(b, k);
                if j < b.len() && b[j] == b',' {
                    j += 1;
                }
            }
        }
        _ => Err(format!("unexpected byte {} at {}", b[i], i)),
    }
}

// ============================================================================
// HTTP client
// ============================================================================

fn run(req: &Request) -> Result<(), String> {
    match req.cmd.as_str() {
        "list" => run_list(req),
        "new" => run_new(req),
        "lookup" => run_lookup(req),
        "read" => run_read(req),
        "send" => run_send(req),
        other => Err(format!("unknown cmd: {}", other)),
    }
}

fn run_list(req: &Request) -> Result<(), String> {
    let body = http(req, "GET", "/v1/mailboxes", None)?;
    let mailboxes = pluck_string_list(&body, "mailboxes");
    if mailboxes.is_empty() {
        out("(no mailboxes registered)\n");
    } else {
        for m in mailboxes {
            out(&format!("{}\n", m));
        }
    }
    Ok(())
}

fn run_new(req: &Request) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("new: --address required")?;
    let body_json = format!("{{\"address\":\"{}\"}}", escape_json(addr));
    let body = http(req, "POST", "/v1/mailboxes", Some(&body_json))?;
    out(&format!("{}\n", body));
    Ok(())
}

fn run_lookup(req: &Request) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("lookup: --address required")?;
    let path = format!("/v1/mailboxes/{}", url_encode(addr));
    let body = http(req, "GET", &path, None)?;
    out(&format!("{}\n", body));
    Ok(())
}

fn run_read(req: &Request) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("read: --address required")?;
    let since = req.since.unwrap_or(0);
    let path = format!("/v1/mailboxes/{}/inbox?since={}", url_encode(addr), since);
    let body = http(req, "GET", &path, None)?;
    print_messages(&body, req.full);
    Ok(())
}

fn run_send(req: &Request) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("send: --address (from) required")?;
    if req.to.is_empty() {
        return Err(String::from("send: at least one --to required"));
    }
    let subject = req.subject.as_deref().unwrap_or("");
    let body_text = req.body.as_deref().unwrap_or("");
    let payload = match req.smtp_server.as_deref() {
        Some(smtp) => format!(
            "{{\"to\":{},\"cc\":{},\"bcc\":{},\"subject\":\"{}\",\"body\":\"{}\",\"smtp_server\":\"{}\"}}",
            json_string_array(&req.to),
            json_string_array(&req.cc),
            json_string_array(&req.bcc),
            escape_json(subject),
            escape_json(body_text),
            escape_json(smtp),
        ),
        None => format!(
            "{{\"to\":{},\"cc\":{},\"bcc\":{},\"subject\":\"{}\",\"body\":\"{}\"}}",
            json_string_array(&req.to),
            json_string_array(&req.cc),
            json_string_array(&req.bcc),
            escape_json(subject),
            escape_json(body_text),
        ),
    };
    let path = format!("/v1/mailboxes/{}/send", url_encode(addr));
    let body = http(req, "POST", &path, Some(&payload))?;
    out(&format!("{}\n", body));
    Ok(())
}

fn json_string_array(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", escape_json(s)))
        .collect();
    format!("[{}]", parts.join(","))
}

/// Talk HTTP/1.1 to `req.api` and return the response body.
///
/// Reads headers until `\r\n\r\n`, parses `Content-Length`, then reads exactly
/// that many body bytes — so we don't depend on the peer doing a graceful TLS
/// close (rustls is strict about unclean shutdowns, but the actual HTTP/1.1
/// semantics let us stop reading once we have the body).
fn http(req: &Request, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let conn = tcp_connect(req.api.clone()).map_err(|e| format!("connect to {}: {}", req.api, e))?;
    let mut http_req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
        method, path, req.api, req.token
    );
    if let Some(b) = body {
        http_req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            b.len(),
            b
        ));
    } else {
        http_req.push_str("\r\n");
    }
    tcp_send(conn.clone(), http_req.into_bytes()).map_err(|e| format!("send: {}", e))?;

    let mut all = Vec::new();
    let mut body_start: Option<usize> = None;
    let mut content_length: Option<usize> = None;

    loop {
        // Stop conditions:
        // - we know body_start and content_length and have everything
        // - peer closed (chunk empty) — accept what we have
        if let (Some(hs), Some(cl)) = (body_start, content_length) {
            if all.len() >= hs + cl {
                break;
            }
        }

        let chunk = match tcp_receive(conn.clone(), 65536) {
            Ok(c) => c,
            Err(e) => {
                // Best effort — if we already have headers+body, return that.
                if let (Some(hs), Some(cl)) = (body_start, content_length) {
                    if all.len() >= hs + cl {
                        break;
                    }
                }
                return Err(format!("recv: {}", e));
            }
        };
        if chunk.is_empty() {
            break;
        }
        all.extend_from_slice(&chunk);

        // Re-scan for end-of-headers + Content-Length on each pass.
        if body_start.is_none() {
            if let Some(idx) = find_subseq(&all, b"\r\n\r\n") {
                body_start = Some(idx + 4);
                let header_str = core::str::from_utf8(&all[..idx]).unwrap_or("");
                for line in header_str.split("\r\n") {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            if let Ok(n) = value.trim().parse::<usize>() {
                                content_length = Some(n);
                            }
                        }
                    }
                }
                if content_length.is_none() {
                    // No length given — read until peer closes.
                    content_length = Some(usize::MAX);
                }
            }
        }
    }

    let _ = tcp_close(conn);

    let text = String::from_utf8(all).map_err(|_| String::from("non-utf8 response"))?;
    let start = body_start.unwrap_or_else(|| text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0));
    let end = match content_length {
        Some(n) if n != usize::MAX => start + n.min(text.len() - start),
        _ => text.len(),
    };
    Ok(text[start..end].to_string())
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

// ============================================================================
// Output helpers
// ============================================================================

fn out(s: &str) {
    let _ = write_stdout(s.as_bytes().to_vec());
}

fn err(s: &str) {
    let _ = write_stderr(s.as_bytes().to_vec());
}

fn print_messages(json: &str, full: bool) {
    // Find each "messages":[ ... ] block; emit one entry per message.
    let msgs = extract_array(json, "messages");
    for m in &msgs {
        let from = pluck_field(m, "from");
        let to = pluck_field(m, "to");
        let subj = pluck_field(m, "subject");
        let id = pluck_number(m, "id");
        let body = pluck_field(m, "body");
        out(&format!(
            "id={}  from={}  to={}  subject={:?}\n",
            id, from, to, subj
        ));
        let display: &str = if full {
            &body
        } else {
            strip_quoted_history(&body)
        };
        let lines_seen = display.split('\n').count();
        for line in display.split('\n').take(20) {
            out(&format!("      {}\n", line));
        }
        if lines_seen > 20 {
            out(&format!("      ... ({} more lines)\n", lines_seen - 20));
        }
        if !full && display.len() < body.len() {
            out("      [quoted history hidden; --full to show]\n");
        }
        out("\n");
    }
    let next = pluck_number(json, "next_cursor");
    out(&format!("next_cursor={}  count={}\n", next, msgs.len()));
}

/// Return the visible portion of a reply body, stripping the quoted
/// history block that gmail / outlook append below the new content.
/// Conservative: only strips when a recognizable separator is matched.
///   - Gmail: a line starting with "On " and ending with " wrote:"
///   - Outlook: a line whose content is exactly "-----Original Message-----"
/// Trailing whitespace/blank lines are trimmed after stripping. Returns
/// the input unchanged if no separator is found.
fn strip_quoted_history(body: &str) -> &str {
    let mut offset = 0usize;
    let mut cut_at: Option<usize> = None;
    for line in body.split('\n') {
        let trimmed = line.trim_end_matches('\r').trim();
        if is_gmail_attribution(trimmed) || is_outlook_separator(trimmed) {
            cut_at = Some(offset);
            break;
        }
        // +1 accounts for the '\n' that split() consumed (only correct
        // when there's a following segment; if this is the last segment
        // we won't read past it).
        offset += line.len() + 1;
    }
    let end = match cut_at {
        Some(i) => i,
        None => return body,
    };
    body[..end].trim_end_matches(|c: char| c == '\n' || c == '\r' || c == ' ' || c == '\t')
}

fn is_gmail_attribution(line: &str) -> bool {
    line.starts_with("On ") && line.ends_with(" wrote:")
}

fn is_outlook_separator(line: &str) -> bool {
    line.eq_ignore_ascii_case("-----Original Message-----")
}

/// Find `"key":"..."` and return the unescaped contents. Tolerant about
/// surrounding whitespace; doesn't handle escape sequences other than
/// `\"`, `\\`, `\n`, `\r`, `\t`.
fn pluck_field(s: &str, key: &str) -> String {
    let needle = format!("\"{}\":", key);
    let Some(i) = s.find(&needle) else {
        return String::new();
    };
    let after = &s[i + needle.len()..];
    let after = after.trim_start();
    if !after.starts_with('"') {
        return String::new();
    }
    let bytes = after.as_bytes();
    let mut out = Vec::new();
    let mut j = 1usize;
    while j < bytes.len() {
        match bytes[j] {
            b'"' => return String::from_utf8(out).unwrap_or_default(),
            b'\\' if j + 1 < bytes.len() => {
                let esc = match bytes[j + 1] {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    other => other,
                };
                out.push(esc);
                j += 2;
            }
            c => {
                out.push(c);
                j += 1;
            }
        }
    }
    String::new()
}

/// `"key":N` where N is a non-negative integer.
fn pluck_number(s: &str, key: &str) -> u64 {
    let needle = format!("\"{}\":", key);
    let Some(i) = s.find(&needle) else { return 0 };
    let after = &s[i + needle.len()..].trim_start();
    let mut n = 0u64;
    for c in after.chars() {
        if let Some(d) = c.to_digit(10) {
            n = n * 10 + d as u64;
        } else {
            break;
        }
    }
    n
}

/// `"key":["a","b","c"]` → vec of strings. Strings only.
fn pluck_string_list(s: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{}\":[", key);
    let Some(i) = s.find(&needle) else {
        return Vec::new();
    };
    let after = &s[i + needle.len()..];
    let Some(end) = after.find(']') else {
        return Vec::new();
    };
    let inner = &after[..end];
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut j = 0;
    while j < bytes.len() {
        // skip whitespace and commas
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b',' || bytes[j] == b'\n') {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        if bytes[j] == b'"' {
            let (s, k) = match parse_string(bytes, j) {
                Ok(v) => v,
                Err(_) => break,
            };
            out.push(s);
            j = k;
        } else {
            break;
        }
    }
    out
}

/// Extract each element of a top-level array under `key`. Returns each
/// element as the raw JSON text (so subsequent pluck_field calls work).
fn extract_array(json: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{}\":[", key);
    let Some(i) = json.find(&needle) else {
        return Vec::new();
    };
    let inner_start = i + needle.len();
    // Find matching ']' by tracking depth
    let bytes = json.as_bytes();
    let mut depth = 1i32;
    let mut j = inner_start;
    while j < bytes.len() && depth > 0 {
        match bytes[j] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b'"' => {
                // skip string
                j += 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
            }
            _ => {}
        }
        j += 1;
    }
    let end = j.saturating_sub(1);
    let inner = &json[inner_start..end];
    // Split top-level objects (by tracking depth).
    let mut out = Vec::new();
    let b = inner.as_bytes();
    let mut depth2 = 0i32;
    let mut start: Option<usize> = None;
    let mut k = 0;
    while k < b.len() {
        match b[k] {
            b'{' => {
                if depth2 == 0 {
                    start = Some(k);
                }
                depth2 += 1;
            }
            b'}' => {
                depth2 -= 1;
                if depth2 == 0 {
                    if let Some(s0) = start {
                        out.push(inner[s0..=k].to_string());
                        start = None;
                    }
                }
            }
            b'"' => {
                k += 1;
                while k < b.len() && b[k] != b'"' {
                    if b[k] == b'\\' && k + 1 < b.len() {
                        k += 2;
                    } else {
                        k += 1;
                    }
                }
            }
            _ => {}
        }
        k += 1;
    }
    out
}

// ============================================================================
// String escaping
// ============================================================================

fn escape_json(s: &str) -> String {
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

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let ok = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'.'
            || byte == b'_'
            || byte == b'~';
        if ok {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}
