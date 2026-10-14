//! inbox — ergonomic native client to the inbox at mail.colinrozzi.com.
//!
//! A plain HTTPS client to the inbox HTTP API (bearer token). NO theater, NO
//! wasm — this replaces the old bash-wrapper + inbox_cli.wasm + `theater spawn`
//! dance (option (a)): the CLI's entire surface was always just HTTP/1.1-over-TLS
//! calls to /v1/mailboxes/*, so there is no reason to carry a wasm runtime + its
//! ABI-skew fragility. Static-musl packaged, released like `supervisor`.
//!
//! Config:
//!   INBOX_API    host[:port] of the API   (default: mail.colinrozzi.com:443)
//!   INBOX_TOKEN  bearer token; required    (alt: ~/.config/inbox/token, one line)

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

const DEFAULT_API: &str = "mail.colinrozzi.com:443";

// Network timeouts for the HTTPS request path in http(). Without these the CLI
// does a blocking connect + blocking rustls read with NO timeout, so a slow or
// unreachable server hangs it indefinitely — in a Monitor poll-loop that's a
// silently-dark agent. Fail fast instead.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
// Bounded retry on the TCP connect ONLY (see connect_with_retry). 1 initial try
// + up to 2 retries, with a short backoff between attempts.
const CONNECT_RETRIES: u32 = 2;
const CONNECT_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(name = "inbox", about = "Client to the inbox mail server HTTP API")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List registered addresses
    List,
    /// Register an address
    New { address: String },
    /// Look up an address
    Lookup { address: String },
    /// Read a mailbox
    Read {
        address: String,
        #[arg(long, default_value_t = 0)]
        since: u64,
        /// Show full bodies (bypass the quoted-history strip)
        #[arg(long)]
        full: bool,
    },
    /// Send a message (--to/--cc/--bcc may each be repeated)
    Send {
        /// The `from` mailbox
        from: String,
        #[command(flatten)]
        recips: Recipients,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        smtp: Option<String>,
        #[arg(long = "in-reply-to")]
        in_reply_to: Option<String>,
        /// Space-separated Message-ID chain
        #[arg(long)]
        references: Option<String>,
    },
    /// Reply to message <id> in <from>'s mailbox (auto-threads + "Re:" subject)
    Reply {
        from: String,
        id: u64,
        #[command(flatten)]
        recips: Recipients,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        smtp: Option<String>,
    },
    /// Forward a received message to one or more recipients
    Forward {
        from: String,
        id: u64,
        #[arg(long = "to", required = true)]
        to: Vec<String>,
        #[arg(long = "cc")]
        cc: Vec<String>,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Args)]
struct Recipients {
    #[arg(long = "to", required = true)]
    to: Vec<String>,
    #[arg(long = "cc")]
    cc: Vec<String>,
    #[arg(long = "bcc")]
    bcc: Vec<String>,
}

// ---- API response shapes (mirror api-handler) ----
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

struct Ctx {
    api: String,
    token: String,
}

fn main() -> ExitCode {
    // rustls 0.23 needs an explicit process-level crypto provider when built with
    // the ring backend and default features off.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();

    let token = match resolve_token() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("inbox: {e}");
            return ExitCode::from(2);
        }
    };
    let ctx = Ctx {
        api: std::env::var("INBOX_API").unwrap_or_else(|_| DEFAULT_API.to_string()),
        token,
    };

    if let Err(e) = run(&ctx, cli.cmd) {
        eprintln!("inbox: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn resolve_token() -> Result<String, String> {
    if let Ok(t) = std::env::var("INBOX_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".config/inbox/token");
        if let Ok(s) = std::fs::read_to_string(&path) {
            let t = s.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }
    Err("set INBOX_TOKEN or write ~/.config/inbox/token".to_string())
}

fn run(ctx: &Ctx, cmd: Command) -> Result<(), String> {
    match cmd {
        Command::List => run_list(ctx),
        Command::New { address } => run_new(ctx, &address),
        Command::Lookup { address } => run_lookup(ctx, &address),
        Command::Read { address, since, full } => run_read(ctx, &address, since, full),
        Command::Send { from, recips, subject, body, smtp, in_reply_to, references } => {
            let payload = SendBody {
                to: &recips.to,
                cc: &recips.cc,
                bcc: &recips.bcc,
                subject: subject.as_deref().unwrap_or(""),
                body: body.as_deref().unwrap_or(""),
                smtp_server: smtp.as_deref(),
                in_reply_to: in_reply_to.as_deref(),
                references: references.as_deref(),
            };
            let resp = http(ctx, "POST", &format!("/v1/mailboxes/{}/send", url_encode(&from)), Some(&to_json(&payload)?))?;
            println!("{resp}");
            Ok(())
        }
        Command::Reply { from, id, recips, subject, body, smtp } => {
            run_reply(ctx, &from, id, &recips, subject.as_deref(), body.as_deref(), smtp.as_deref())
        }
        Command::Forward { from, id, to, cc, note } => {
            run_forward(ctx, &from, id, &to, &cc, note.as_deref())
        }
    }
}

fn run_list(ctx: &Ctx) -> Result<(), String> {
    let body = http(ctx, "GET", "/v1/mailboxes", None)?;
    let resp: MailboxList = parse(&body, "/v1/mailboxes")?;
    if resp.mailboxes.is_empty() {
        println!("(no mailboxes registered)");
    } else {
        for m in resp.mailboxes {
            println!("{m}");
        }
    }
    Ok(())
}

fn run_new(ctx: &Ctx, addr: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body<'a> {
        address: &'a str,
    }
    let body = http(ctx, "POST", "/v1/mailboxes", Some(&to_json(&Body { address: addr })?))?;
    println!("{body}");
    Ok(())
}

fn run_lookup(ctx: &Ctx, addr: &str) -> Result<(), String> {
    let body = http(ctx, "GET", &format!("/v1/mailboxes/{}", url_encode(addr)), None)?;
    println!("{body}");
    Ok(())
}

fn run_read(ctx: &Ctx, addr: &str, since: u64, full: bool) -> Result<(), String> {
    let path = format!("/v1/mailboxes/{}/inbox?since={}", url_encode(addr), since);
    let body = http(ctx, "GET", &path, None)?;
    let page: InboxPage = parse(&body, "/inbox")?;
    print_messages(&page, full);
    Ok(())
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

fn run_reply(
    ctx: &Ctx,
    from: &str,
    id: u64,
    recips: &Recipients,
    subject: Option<&str>,
    body: Option<&str>,
    smtp: Option<&str>,
) -> Result<(), String> {
    let page: InboxPage = parse(
        &http(ctx, "GET", &format!("/v1/mailboxes/{}/inbox?since=0", url_encode(from)), None)?,
        "/inbox",
    )?;
    let original = page
        .messages
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("no message id={id} in {from}'s mailbox"))?;

    let new_subject = match subject.filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            if original.subject.to_ascii_lowercase().starts_with("re:") {
                original.subject.clone()
            } else {
                format!("Re: {}", original.subject)
            }
        }
    };

    let parent_id = original.message_id.trim();
    let in_reply_to = if parent_id.is_empty() { None } else { Some(ensure_angle(parent_id)) };
    let references = build_references(&original.references, parent_id);

    let payload = SendBody {
        to: &recips.to,
        cc: &recips.cc,
        bcc: &recips.bcc,
        subject: &new_subject,
        body: body.unwrap_or(""),
        smtp_server: smtp,
        in_reply_to: in_reply_to.as_deref(),
        references: references.as_deref(),
    };
    let resp = http(ctx, "POST", &format!("/v1/mailboxes/{}/send", url_encode(from)), Some(&to_json(&payload)?))?;
    println!("{resp}");
    Ok(())
}

fn run_forward(ctx: &Ctx, from: &str, id: u64, to: &[String], cc: &[String], note: Option<&str>) -> Result<(), String> {
    let page: InboxPage = parse(
        &http(ctx, "GET", &format!("/v1/mailboxes/{}/inbox?since=0", url_encode(from)), None)?,
        "/inbox",
    )?;
    let original = page
        .messages
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("no message id={id} in {from}'s mailbox"))?;

    let new_subject = if original.subject.starts_with("Fwd:") || original.subject.starts_with("Fw:") {
        original.subject.clone()
    } else {
        format!("Fwd: {}", original.subject)
    };

    let mut fbody = String::new();
    if let Some(n) = note.filter(|n| !n.is_empty()) {
        fbody.push_str(n);
        fbody.push_str("\n\n");
    }
    fbody.push_str("---------- Forwarded message ----------\n");
    fbody.push_str(&format!("From: {}\n", original.from));
    if original.received_at != 0 {
        fbody.push_str(&format!("Date: {} (epoch ms)\n", original.received_at));
    }
    fbody.push_str(&format!("Subject: {}\n\n", original.subject));
    fbody.push_str(&original.body);

    #[derive(Serialize)]
    struct FwdBody<'a> {
        to: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        cc: &'a [String],
        subject: &'a str,
        body: &'a str,
    }
    let payload = FwdBody { to, cc, subject: &new_subject, body: &fbody };
    let resp = http(ctx, "POST", &format!("/v1/mailboxes/{}/send", url_encode(from)), Some(&to_json(&payload)?))?;
    println!("{resp}");
    Ok(())
}

// ---- HTTP (ureq 2.x, blocking, rustls) ----
// Talk HTTP/1.1 to the inbox API over TLS (rustls) and return the response body.
//
// The ENTIRE request (request line + headers + Content-Length + body) is built
// into one buffer and written with a single write_all, so a small request goes
// out as a single TLS record. This is REQUIRED: the api-handler reads the whole
// request with a single tcp_receive, so headers + body must arrive together.
// (ureq wrote the POST body as a separate write -> a separate TLS record, which
// the server's single read missed -> empty body -> 400. This mirrors the framing
// the old wasm CLI used.) Response is read to EOF (Connection: close).
fn http(ctx: &Ctx, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let (host, port) = match ctx.api.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("invalid port in INBOX_API: {}", ctx.api))?,
        ),
        None => (ctx.api.clone(), 443),
    };

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
        ctx.token
    );
    match body {
        Some(b) => req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            b.len(),
            b
        )),
        None => req.push_str("\r\n"),
    }

    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|_| format!("invalid host: {host}"))?;
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
        .map_err(|e| format!("tls setup: {e}"))?;

    // Resolve host:port to concrete socket addresses. connect_timeout() needs a
    // SocketAddr (unlike plain connect(), which resolves internally), and having
    // the list lets us try each address with the per-attempt connect timeout.
    let addrs: Vec<std::net::SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("resolve {host}:{port}: no addresses"));
    }

    // Connect with a bounded retry. CRITICAL: the retry wraps the CONNECT ONLY —
    // no request bytes have been written at this point. We deliberately do NOT
    // retry the write/read below: the inbox /send path is NON-IDEMPOTENT, so
    // resending a request whose bytes already reached the server would duplicate
    // mail. A request must be sent AT MOST ONCE; a failed TCP connect is the only
    // phase safe to retry, precisely because nothing has been sent yet.
    let mut sock = connect_with_retry(
        CONNECT_RETRIES,
        CONNECT_BACKOFF,
        &mut |d| std::thread::sleep(d),
        || {
            let mut last_err = None;
            for addr in &addrs {
                match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
                    Ok(s) => return Ok(s),
                    Err(e) => last_err = Some(e),
                }
            }
            // addrs is non-empty (checked above), so the loop set last_err.
            Err(last_err.expect("addrs non-empty => at least one attempt"))
        },
    )
    .map_err(|e| format!("connect {host}:{port}: {e}"))?;

    // READ/WRITE timeouts so a server that accepts the connection but then stalls
    // mid-handshake or mid-response can't wedge us forever. Set BEFORE the rustls
    // handshake so the handshake's own I/O is covered too, not just the body read.
    sock.set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("set read timeout: {e}"))?;
    sock.set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("set write timeout: {e}"))?;

    let mut tls = rustls::Stream::new(&mut conn, &mut sock);

    tls.write_all(req.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;
    let _ = tls.flush();

    // Connection: close -> read to EOF. TCP is a byte stream: one read() returns only
    // what's currently available, so a SINGLE read would TRUNCATE any response bigger
    // than one segment/record (the exact class of bug that bit non-looping relay
    // readers on large /inbox responses). read_to_eof loops until EOF; see its test.
    let buf = read_to_eof(&mut tls)?;

    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, resp_body) = text.split_once("\r\n\r\n").ok_or_else(|| {
        format!(
            "malformed response: {}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    let code: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if !(200..300).contains(&code) {
        return Err(format!("HTTP {code}: {}", resp_body.trim()));
    }
    Ok(resp_body.to_string())
}

fn to_json<T: Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| format!("encode body: {e}"))
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str, what: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| format!("parse {what} response: {e}"))
}

// ============================================================================
// Pure logic — ported verbatim from the wasm CLI (behavior-preserving).
// ============================================================================

fn print_messages(page: &InboxPage, full: bool) {
    for m in &page.messages {
        println!("id={}  from={}  to={}  subject={:?}", m.id, m.from, m.to, m.subject);
        let display: &str = if full { &m.body } else { strip_quoted_history(&m.body) };
        let lines_seen = display.split('\n').count();
        let cap = if full { usize::MAX } else { 120 };
        for line in display.split('\n').take(cap) {
            println!("      {line}");
        }
        if lines_seen > cap {
            println!("      ... ({} more lines; --full to show)", lines_seen - cap);
        }
        if !full && display.len() < m.body.len() {
            println!("      [quoted history hidden; --full to show]");
        }
        println!();
    }
    println!("next_cursor={}  count={}", page.next_cursor, page.messages.len());
}

/// Strip the quoted-history block gmail/outlook append below new content.
/// Conservative: only strips on a recognizable separator.
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

/// Wrap a bare message-id in angle brackets if it isn't already.
fn ensure_angle(id: &str) -> String {
    let t = id.trim();
    if t.starts_with('<') && t.ends_with('>') {
        t.to_string()
    } else {
        format!("<{t}>")
    }
}

/// Reply's References: parent's References tokens + parent's Message-ID.
fn build_references(parent_refs: &str, parent_msgid: &str) -> Option<String> {
    let mut parts: Vec<String> = parent_refs.split_whitespace().map(|s| s.to_string()).collect();
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

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let ok = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if ok {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// Try `connect` up to `1 + retries` times, sleeping `backoff` between attempts;
// return the first Ok, or the LAST Err if every attempt fails.
//
// CRITICAL — at-most-once discipline: the caller must pass a closure that does
// ONLY the TCP connect (no request write). Retrying is safe here precisely
// because no request bytes have been sent when connect fails; the inbox /send
// path is non-idempotent, so this must never be used to re-drive a write/read.
// `sleep` is injected so this logic is unit-testable without real delays.
fn connect_with_retry<T, E>(
    retries: u32,
    backoff: Duration,
    sleep: &mut dyn FnMut(Duration),
    mut connect: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut attempt = 0u32;
    loop {
        match connect() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= retries {
                    return Err(e);
                }
                sleep(backoff);
                attempt += 1;
            }
        }
    }
}

/// Read a byte-stream to EOF, accumulating every chunk. A single `read()` on a TCP
/// stream returns only what is currently available (often one segment/TLS record), so
/// reading a large response REQUIRES this loop — a single read truncates anything
/// bigger than one read. Mirrors the server-side read_full_request loop, on the
/// response-read side. Extracted so the multi-record reassembly is unit-tested.
fn read_to_eof<R: Read>(r: &mut R) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            // A read timeout (set_read_timeout) surfaces as WouldBlock/TimedOut. This
            // means the server STALLED mid-response, not that it finished — treat it
            // as an error even if we already have bytes, so we fail fast with a clear
            // message rather than silently returning a TRUNCATED body to the parser.
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(format!("read response timed out after {IO_TIMEOUT:?}"));
            }
            // Tolerate an unclean TLS close (peer drops without close_notify) once we
            // already have bytes — the body is complete, just not cleanly shut down.
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                if buf.is_empty() {
                    return Err(format!("read response: {e}"));
                }
                break;
            }
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::{connect_with_retry, read_to_eof};
    use std::cell::Cell;
    use std::io::{self, Read};
    use std::time::Duration;

    // A connect that fails `fail_n` times then succeeds must be driven at most
    // `1 + retries` times and sleep once before each retry — bounded, never
    // unbounded. This encodes the at-most-once discipline: the retry count is a
    // hard ceiling.
    #[test]
    fn connect_with_retry_succeeds_after_bounded_failures() {
        let calls = Cell::new(0u32);
        let mut slept = 0u32;
        let out: Result<&str, &str> = connect_with_retry(
            2,
            Duration::from_millis(1),
            &mut |_| slept += 1,
            || {
                let n = calls.get();
                calls.set(n + 1);
                if n < 2 {
                    Err("refused")
                } else {
                    Ok("connected")
                }
            },
        );
        assert_eq!(out, Ok("connected"));
        assert_eq!(calls.get(), 3, "1 initial attempt + 2 retries");
        assert_eq!(slept, 2, "slept once before each of the 2 retries");
    }

    // When every attempt fails, it gives up after exactly `1 + retries` tries and
    // returns the last error — it must NOT loop forever.
    #[test]
    fn connect_with_retry_gives_up_after_ceiling() {
        let calls = Cell::new(0u32);
        let mut slept = 0u32;
        let out: Result<&str, &str> = connect_with_retry(
            2,
            Duration::from_millis(1),
            &mut |_| slept += 1,
            || {
                calls.set(calls.get() + 1);
                Err("refused")
            },
        );
        assert_eq!(out, Err("refused"));
        assert_eq!(calls.get(), 3, "attempts capped at 1 + retries");
        assert_eq!(slept, 2);
    }

    // retries = 0 means exactly one attempt and no sleep — the degenerate bound.
    #[test]
    fn connect_with_retry_zero_retries_tries_once() {
        let calls = Cell::new(0u32);
        let mut slept = 0u32;
        let out: Result<&str, &str> = connect_with_retry(
            0,
            Duration::from_millis(1),
            &mut |_| slept += 1,
            || {
                calls.set(calls.get() + 1);
                Err("refused")
            },
        );
        assert_eq!(out, Err("refused"));
        assert_eq!(calls.get(), 1);
        assert_eq!(slept, 0);
    }

    // A read timeout (WouldBlock) mid-body must ERROR, not silently return the
    // partial bytes it had buffered — otherwise a stalled server yields a
    // truncated response the parser would mis-handle.
    #[test]
    fn read_to_eof_errors_on_read_timeout_even_with_partial_bytes() {
        struct OneChunkThenTimeout {
            sent: bool,
        }
        impl Read for OneChunkThenTimeout {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if !self.sent {
                    self.sent = true;
                    let data = b"partial";
                    out[..data.len()].copy_from_slice(data);
                    return Ok(data.len());
                }
                Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"))
            }
        }
        let err = read_to_eof(&mut OneChunkThenTimeout { sent: false })
            .expect_err("timeout mid-body must error, not truncate");
        assert!(err.contains("timed out"), "unexpected error: {err}");
    }

    // Hands back its data in FIXED-SIZE pieces, one per read() call, then EOF —
    // simulating a multi-segment TCP response where a single read() returns far less
    // than the whole body.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        piece: usize,
    }
    impl Read for ChunkedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0); // EOF
            }
            let end = (self.pos + self.piece).min(self.data.len());
            let n = (end - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    // REGRESSION: a ~1.2MB body delivered 1500 bytes per read() (TCP-segment-sized,
    // many iterations, smaller than the 8192 chunk) must reassemble WHOLE. A single
    // read (the truncation bug class) would return only the first 1500 bytes.
    #[test]
    fn read_to_eof_reassembles_multi_record_body() {
        let body: Vec<u8> = (0..1_200_000u32).map(|i| (i % 251) as u8).collect();
        let mut r = ChunkedReader { data: body.clone(), pos: 0, piece: 1500 };
        let got = read_to_eof(&mut r).expect("read_to_eof");
        assert_eq!(got.len(), body.len(), "truncated: got {} of {} bytes", got.len(), body.len());
        assert_eq!(got, body, "reassembled bytes differ from original");
    }

    // The unclean-EOF-with-bytes tolerance (peer drops without close_notify after the
    // body) must still return the bytes, not error.
    #[test]
    fn read_to_eof_tolerates_unclean_eof_with_bytes() {
        struct OneChunkThenUncleanEof {
            sent: bool,
        }
        impl Read for OneChunkThenUncleanEof {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if !self.sent {
                    self.sent = true;
                    let data = b"partial-but-complete-body";
                    out[..data.len()].copy_from_slice(data);
                    return Ok(data.len());
                }
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no close_notify"))
            }
        }
        let got = read_to_eof(&mut OneChunkThenUncleanEof { sent: false }).expect("tolerate unclean eof");
        assert_eq!(got, b"partial-but-complete-body");
    }
}
