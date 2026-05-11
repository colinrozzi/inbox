//! DKIM signing (RFC 6376) for outbound mail.
//!
//! Algorithm: rsa-sha256, relaxed/relaxed canonicalization, selector `default`,
//! domain `colinrozzi.com`. Deterministic PKCS#1 v1.5 signing — no RNG needed.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use base64::engine::{general_purpose::STANDARD as B64, Engine as _};
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use sha2::{Digest, Sha256};

pub const SELECTOR: &str = "default";
pub const DOMAIN: &str = "colinrozzi.com";

/// Sign one outbound message. Returns the `DKIM-Signature: ...\r\n` header
/// to prepend to the message before transmission.
///
/// `headers_str` is the full headers block (each line CRLF-terminated, no
/// blank line at the end). `body` is the message body in CRLF form.
/// `signed_headers` lists the lowercase header names to include in `h=`.
pub fn sign_message(
    private_key_pem: &str,
    selector: &str,
    domain: &str,
    signed_headers: &[&str],
    headers_str: &str,
    body: &[u8],
) -> Result<String, String> {
    let private_key = RsaPrivateKey::from_pkcs8_pem(private_key_pem)
        .map_err(|e| format!("dkim: parse private key: {}", e))?;

    let body_hash = body_hash_relaxed(body);

    // h= value, e.g. "from:to:subject"
    let h_value = signed_headers.join(":");

    // DKIM-Signature header WITHOUT the b= value. Field names use the
    // RFC 6376 spelling. Whitespace is minimal; relaxed canonicalization
    // strips runs anyway.
    let dkim_header_unsigned = format!(
        "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed; d={domain}; s={selector}; h={h}; bh={bh}; b=",
        domain = domain,
        selector = selector,
        h = h_value,
        bh = body_hash,
    );

    // Build the canonicalized header set we sign: each signed header in
    // h= order, then the DKIM-Signature header itself, all relaxed-canon.
    // Per RFC 6376 §3.7, the DKIM-Signature header is canonicalized with an
    // empty b= value and is NOT terminated by CRLF.
    let mut to_sign = String::new();
    for name in signed_headers {
        if let Some(value) = find_header(headers_str, name) {
            to_sign.push_str(&canon_header_relaxed(name, &value));
        }
    }
    to_sign.push_str(&canon_header_relaxed_no_crlf(
        "DKIM-Signature",
        dkim_header_unsigned
            .strip_prefix("DKIM-Signature: ")
            .unwrap_or(&dkim_header_unsigned),
    ));

    let signing_key = SigningKey::<Sha256>::new(private_key);
    let signature = signing_key.sign(to_sign.as_bytes());
    let b_value = B64.encode(signature.to_bytes());

    Ok(format!("{}{}\r\n", dkim_header_unsigned, b_value))
}

/// Find the value of `name` (case-insensitive) in a headers block.
/// Handles header folding (continuation lines starting with WSP).
fn find_header(headers_str: &str, name: &str) -> Option<String> {
    let target = name.to_ascii_lowercase();
    let mut lines = headers_str.split("\r\n").peekable();
    while let Some(line) = lines.next() {
        if let Some(colon) = line.find(':') {
            if line[..colon].trim().eq_ignore_ascii_case(&target) {
                let mut value = line[colon + 1..].to_string();
                // Pull in any continuation lines.
                while let Some(peek) = lines.peek() {
                    if peek.starts_with(' ') || peek.starts_with('\t') {
                        value.push_str("\r\n");
                        value.push_str(peek);
                        lines.next();
                    } else {
                        break;
                    }
                }
                return Some(value);
            }
        }
    }
    None
}

/// Relaxed header canonicalization (RFC 6376 §3.4.2), returning the
/// canonicalized header line terminated with CRLF.
fn canon_header_relaxed(name: &str, value: &str) -> String {
    let mut out = canon_header_relaxed_no_crlf(name, value);
    out.push_str("\r\n");
    out
}

/// Same as `canon_header_relaxed` but no trailing CRLF. Used for the
/// DKIM-Signature header per RFC 6376 §3.7.
fn canon_header_relaxed_no_crlf(name: &str, value: &str) -> String {
    let canon_name = name.to_ascii_lowercase();
    let canon_value = canon_header_value(value);
    format!("{}:{}", canon_name, canon_value)
}

/// Relaxed value canonicalization: unfold continuations, collapse runs
/// of WSP into a single SP, strip leading and trailing WSP.
fn canon_header_value(value: &str) -> String {
    let unfolded: String = value
        .split("\r\n")
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::with_capacity(unfolded.len());
    let mut last_was_wsp = false;
    for c in unfolded.chars() {
        if c == ' ' || c == '\t' {
            if !last_was_wsp {
                out.push(' ');
                last_was_wsp = true;
            }
        } else {
            out.push(c);
            last_was_wsp = false;
        }
    }
    out.trim().to_string()
}

/// Relaxed body canonicalization (RFC 6376 §3.4.4) then SHA-256 + base64.
fn body_hash_relaxed(body: &[u8]) -> String {
    let s = core::str::from_utf8(body).unwrap_or("");
    // Per-line: strip trailing WSP, collapse runs of internal WSP to single SP.
    let mut lines: Vec<String> = Vec::new();
    for line in s.split("\r\n") {
        let mut out = String::with_capacity(line.len());
        let mut last_was_wsp = false;
        for c in line.chars() {
            if c == ' ' || c == '\t' {
                if !last_was_wsp {
                    out.push(' ');
                    last_was_wsp = true;
                }
            } else {
                out.push(c);
                last_was_wsp = false;
            }
        }
        while out.ends_with(' ') {
            out.pop();
        }
        lines.push(out);
    }
    // Rejoin, then strip trailing empty lines so the body ends with one CRLF
    // (or is the empty string if it was all empty lines).
    let mut canon = lines.join("\r\n");
    while canon.ends_with("\r\n\r\n") {
        canon.truncate(canon.len() - 2);
    }
    if !canon.is_empty() && !canon.ends_with("\r\n") {
        canon.push_str("\r\n");
    }

    let mut hasher = Sha256::new();
    hasher.update(canon.as_bytes());
    B64.encode(hasher.finalize())
}
