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
use serde::{Deserialize, Serialize};

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

    let req: CliCommand = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            err(&format!("cli: parse error: {}\n", e));
            shutdown_now();
            return Ok((false, ()));
        }
    };

    if req.cmd.is_empty() {
        err("cli: missing 'cmd' field\n");
        shutdown_now();
        return Ok((false, ()));
    }
    if req.token.is_empty() {
        err("cli: missing 'token' field (set INBOX_TOKEN)\n");
        shutdown_now();
        return Ok((false, ()));
    }

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
// CLI command shape — what the bash wrapper hands us in initial_state.
// ============================================================================

#[derive(Deserialize)]
struct CliCommand {
    #[serde(default)]
    cmd: String,
    #[serde(default = "default_api")]
    api: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    address: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_list")]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    smtp_server: Option<String>,
    #[serde(default)]
    since: u64,
    #[serde(default)]
    full: bool,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    in_reply_to: Option<String>,
    #[serde(default)]
    references: Option<String>,
}

fn default_api() -> String {
    String::from("mail.colinrozzi.com:443")
}

/// `to` accepts either a bare string or a list of strings (back-compat
/// with PR #1's API contract for `/send`). For other commands the field
/// is unused and stays empty.
fn deserialize_string_or_list<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        String(String),
        List(Vec<String>),
    }
    match Option::<Either>::deserialize(d)? {
        None => Ok(Vec::new()),
        Some(Either::String(s)) if s.is_empty() => Ok(Vec::new()),
        Some(Either::String(s)) => Ok(alloc::vec![s]),
        Some(Either::List(l)) => Ok(l),
    }
}

// ============================================================================
// API response shapes (read side). Mirrors what api-handler emits.
// ============================================================================

#[derive(Deserialize)]
struct MailboxList {
    #[serde(default)]
    mailboxes: Vec<String>,
}

#[derive(Deserialize)]
struct InboxPage {
    #[serde(default)]
    messages: Vec<InboxMessage>,
    #[serde(default)]
    next_cursor: u64,
}

#[derive(Deserialize)]
struct InboxMessage {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    received_at: u64,
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    references: String,
}

// ============================================================================
// Cmd dispatch
// ============================================================================

fn run(req: &CliCommand) -> Result<(), String> {
    match req.cmd.as_str() {
        "list" => run_list(req),
        "new" => run_new(req),
        "lookup" => run_lookup(req),
        "read" => run_read(req),
        "send" => run_send(req),
        "reply" => run_reply(req),
        "forward" => run_forward(req),
        other => Err(format!("unknown cmd: {}", other)),
    }
}

fn run_list(req: &CliCommand) -> Result<(), String> {
    let body = http(req, "GET", "/v1/mailboxes", None)?;
    let resp: MailboxList = serde_json::from_str(&body)
        .map_err(|e| format!("parse /v1/mailboxes response: {}", e))?;
    if resp.mailboxes.is_empty() {
        out("(no mailboxes registered)\n");
    } else {
        for m in resp.mailboxes {
            out(&format!("{}\n", m));
        }
    }
    Ok(())
}

fn run_new(req: &CliCommand) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("new: --address required")?;
    #[derive(Serialize)]
    struct Body<'a> {
        address: &'a str,
    }
    let body_json = serde_json::to_string(&Body { address: addr })
        .map_err(|e| format!("encode body: {}", e))?;
    let body = http(req, "POST", "/v1/mailboxes", Some(&body_json))?;
    out(&format!("{}\n", body));
    Ok(())
}

fn run_lookup(req: &CliCommand) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("lookup: --address required")?;
    let path = format!("/v1/mailboxes/{}", url_encode(addr));
    let body = http(req, "GET", &path, None)?;
    out(&format!("{}\n", body));
    Ok(())
}

fn run_read(req: &CliCommand) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("read: --address required")?;
    let path = format!("/v1/mailboxes/{}/inbox?since={}", url_encode(addr), req.since);
    let body = http(req, "GET", &path, None)?;
    let page: InboxPage = serde_json::from_str(&body)
        .map_err(|e| format!("parse /inbox response: {}", e))?;
    print_messages(&page, req.full);
    Ok(())
}

fn run_send(req: &CliCommand) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("send: --address (from) required")?;
    if req.to.is_empty() {
        return Err(String::from("send: at least one --to required"));
    }

    #[derive(Serialize)]
    struct SendBody<'a> {
        to: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        cc: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        bcc: &'a [String],
        subject: &'a str,
        body: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        smtp_server: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        references: Option<&'a str>,
    }

    let body_json = serde_json::to_string(&SendBody {
        to: &req.to,
        cc: &req.cc,
        bcc: &req.bcc,
        subject: req.subject.as_deref().unwrap_or(""),
        body: req.body.as_deref().unwrap_or(""),
        smtp_server: req.smtp_server.as_deref(),
        in_reply_to: req.in_reply_to.as_deref(),
        references: req.references.as_deref(),
    })
    .map_err(|e| format!("encode body: {}", e))?;

    let path = format!("/v1/mailboxes/{}/send", url_encode(addr));
    let body = http(req, "POST", &path, Some(&body_json))?;
    out(&format!("{}\n", body));
    Ok(())
}

/// `./cli/inbox reply <from> <id> --to T... [--cc C...] [--bcc B...]
/// [--subject S] [--body B] [--smtp HOST:PORT]`.
///
/// Reply to message `<id>` in `<from>`'s mailbox, auto-filling the RFC 5322
/// threading chain so the reply lands in the same conversation. `In-Reply-To`
/// becomes the original's `Message-ID`; `References` becomes the original's
/// `References` plus its `Message-ID`. Subject defaults to `Re: <original>`
/// unless `--subject` overrides it. Routing reuses the normal /send path.
fn run_reply(req: &CliCommand) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("reply: <from> required")?;
    if req.to.is_empty() {
        return Err(String::from("reply: at least one --to required"));
    }

    // Pull the original from the replier's mailbox (since=0 scans everything;
    // cheap for typical inbox sizes, same pattern as `forward`).
    let inbox_path = format!("/v1/mailboxes/{}/inbox?since=0", url_encode(addr));
    let body = http(req, "GET", &inbox_path, None)?;
    let page: InboxPage = serde_json::from_str(&body)
        .map_err(|e| format!("parse /inbox response: {}", e))?;
    let original = page
        .messages
        .iter()
        .find(|m| m.id == req.id)
        .ok_or_else(|| format!("no message id={} in {}'s mailbox", req.id, addr))?;

    // Subject: honor an explicit --subject, else prefix Re: unless already there.
    let new_subject = match req.subject.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            if original.subject.to_ascii_lowercase().starts_with("re:") {
                original.subject.clone()
            } else {
                format!("Re: {}", original.subject)
            }
        }
    };

    // Threading chain. Missing parent Message-ID (e.g. a pre-migration
    // message) just means no chain — the send still goes, it just won't group.
    let parent_id = original.message_id.trim();
    let in_reply_to = if parent_id.is_empty() {
        None
    } else {
        Some(ensure_angle(parent_id))
    };
    let references = build_references(&original.references, parent_id);

    #[derive(Serialize)]
    struct SendBody<'a> {
        to: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        cc: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        bcc: &'a [String],
        subject: &'a str,
        body: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        smtp_server: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        references: Option<&'a str>,
    }

    let body_json = serde_json::to_string(&SendBody {
        to: &req.to,
        cc: &req.cc,
        bcc: &req.bcc,
        subject: &new_subject,
        body: req.body.as_deref().unwrap_or(""),
        smtp_server: req.smtp_server.as_deref(),
        in_reply_to: in_reply_to.as_deref(),
        references: references.as_deref(),
    })
    .map_err(|e| format!("encode body: {}", e))?;

    let send_path = format!("/v1/mailboxes/{}/send", url_encode(addr));
    let resp = http(req, "POST", &send_path, Some(&body_json))?;
    out(&format!("{}\n", resp));
    Ok(())
}

/// Wrap a bare message-id in angle brackets if it isn't already. RFC 5322
/// msg-ids in In-Reply-To / References are always angle-bracketed.
fn ensure_angle(id: &str) -> String {
    let t = id.trim();
    if t.starts_with('<') && t.ends_with('>') {
        t.to_string()
    } else {
        format!("<{}>", t)
    }
}

/// Build the reply's `References` value: the parent's References (each token
/// preserved) followed by the parent's own Message-ID. Returns `None` when
/// there's nothing to chain.
fn build_references(parent_refs: &str, parent_msgid: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for tok in parent_refs.split_whitespace() {
        parts.push(tok.to_string());
    }
    let pid = parent_msgid.trim();
    if !pid.is_empty() {
        parts.push(ensure_angle(pid));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// `./cli/inbox forward <from> <id> --to T... [--cc C...] [--note N]`.
///
/// Fetch message `<id>` from `<from>`'s inbox, then send a new message
/// with subject "Fwd: <original>" and a body that reproduces the original
/// content under an informal `---------- Forwarded message ----------`
/// header block (From + Date + Subject). The optional `--note` appears
/// above the separator. Routing reuses the normal /send path so the
/// per-domain MX dispatch works.
fn run_forward(req: &CliCommand) -> Result<(), String> {
    let addr = req.address.as_ref().ok_or("forward: <from> required")?;
    if req.to.is_empty() {
        return Err(String::from("forward: at least one --to required"));
    }

    // Pull the message from the forwarder's mailbox. since=0 returns
    // everything; we scan for the requested id. Cheap for typical inbox
    // sizes; a future API change could allow a direct id lookup.
    let inbox_path = format!("/v1/mailboxes/{}/inbox?since=0", url_encode(addr));
    let body = http(req, "GET", &inbox_path, None)?;
    let page: InboxPage = serde_json::from_str(&body)
        .map_err(|e| format!("parse /inbox response: {}", e))?;
    let original = page
        .messages
        .iter()
        .find(|m| m.id == req.id)
        .ok_or_else(|| format!("no message id={} in {}'s mailbox", req.id, addr))?;

    let new_subject = if original.subject.starts_with("Fwd:")
        || original.subject.starts_with("Fw:")
    {
        original.subject.clone()
    } else {
        format!("Fwd: {}", original.subject)
    };

    let mut forwarded_body = String::new();
    if let Some(n) = req.note.as_deref() {
        if !n.is_empty() {
            forwarded_body.push_str(n);
            forwarded_body.push_str("\n\n");
        }
    }
    forwarded_body.push_str("---------- Forwarded message ----------\n");
    forwarded_body.push_str(&format!("From: {}\n", original.from));
    if original.received_at != 0 {
        // Informal — epoch ms is enough for a forwarded note; receivers
        // who care can parse it. (The CLI doesn't pull in a date crate
        // just for this.)
        forwarded_body.push_str(&format!(
            "Date: {} (epoch ms)\n",
            original.received_at
        ));
    }
    forwarded_body.push_str(&format!("Subject: {}\n", original.subject));
    forwarded_body.push_str("\n");
    forwarded_body.push_str(&original.body);

    #[derive(Serialize)]
    struct SendBody<'a> {
        to: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        cc: &'a [String],
        subject: &'a str,
        body: &'a str,
    }

    let body_json = serde_json::to_string(&SendBody {
        to: &req.to,
        cc: &req.cc,
        subject: &new_subject,
        body: &forwarded_body,
    })
    .map_err(|e| format!("encode body: {}", e))?;

    let send_path = format!("/v1/mailboxes/{}/send", url_encode(addr));
    let resp = http(req, "POST", &send_path, Some(&body_json))?;
    out(&format!("{}\n", resp));
    Ok(())
}

/// Talk HTTP/1.1 to `req.api` and return the response body.
///
/// Reads headers until `\r\n\r\n`, parses `Content-Length`, then reads exactly
/// that many body bytes — so we don't depend on the peer doing a graceful TLS
/// close (rustls is strict about unclean shutdowns, but the actual HTTP/1.1
/// semantics let us stop reading once we have the body).
fn http(req: &CliCommand, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
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

fn print_messages(page: &InboxPage, full: bool) {
    for m in &page.messages {
        out(&format!(
            "id={}  from={}  to={}  subject={:?}\n",
            m.id, m.from, m.to, m.subject
        ));
        let display: &str = if full {
            &m.body
        } else {
            strip_quoted_history(&m.body)
        };
        let lines_seen = display.split('\n').count();
        let cap = if full { usize::MAX } else { 120 };
        for line in display.split('\n').take(cap) {
            out(&format!("      {}\n", line));
        }
        if lines_seen > cap {
            out(&format!("      ... ({} more lines; --full to show)\n", lines_seen - cap));
        }
        if !full && display.len() < m.body.len() {
            out("      [quoted history hidden; --full to show]\n");
        }
        out("\n");
    }
    out(&format!(
        "next_cursor={}  count={}\n",
        page.next_cursor,
        page.messages.len()
    ));
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
