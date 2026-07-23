//! bindings-add: a one-shot Theater ops actor that adds addresses to the inbox
//! router-bindings blob in the store DIRECTLY, without mutating a live router.
//!
//! WHY THIS EXISTS: re-flip attempts dropped claude@ / manager@ / theater-dev@
//! from the router-bindings blob (a mailbox killed mid-spawn drops the accessed
//! address). A live `POST /v1/mailboxes` (register) on the running spine carries
//! the wedge risk that took the spine down, so recovery is instead done offline:
//! the manager spawns THIS actor during the 0.11.0 spine-flip staging (router
//! NOT running), it re-adds the addresses to the persisted blob, and when the
//! fresh 0.11.0 router starts it loads them from the blob (the lazy-spawn router
//! maps a loaded binding with an empty mailbox_id to "known, spawn on first
//! access" — identical to every surviving address).
//!
//! SAFETY:
//!   * DRY-RUN BY DEFAULT. A plain "addr1,addr2,..." initial_state only reports
//!     what it WOULD do and never writes. You must prefix the whole string with
//!     "WRITE:" to actually repoint the label.
//!   * ROUND-TRIP SELF-CHECK: before trusting its own encoder, it decodes the
//!     current blob and RE-ENCODES it unchanged; if the bytes are not identical
//!     it ABORTS (its packr encoding does not reproduce the stored format, so a
//!     write could corrupt the blob). This also proves the 0.11.0 stack can read
//!     the baseline-written blob at all.
//!   * IDEMPOTENT: an address already present is skipped, not duplicated.
//!   * The store-at-label write creates a NEW content-addressed blob and repoints
//!     the label; the OLD blob is untouched, so revert = repoint the label back
//!     to the OLD ref (logged as OLD_REF) — the manager should record it.
//!
//! Types (Binding, STORE_ID, BINDINGS_LABEL) and the encode/decode paths are
//! copied VERBATIM from mailbox-router/src/lib.rs so the wire format matches.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";
const BINDINGS_LABEL: &str = "router-bindings";

// VERBATIM from mailbox-router — do not change the field order/types or the wire
// format diverges from what the router reads.
#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct Binding {
    pub address: String,
    pub mailbox_id: String,
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct ToolState {
    pub done: bool,
}

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
        }
        theater:simple/store {
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<tool-state, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/store", name = "get")]
fn store_get(store_id: String, content_ref: String) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/store", name = "get-by-label")]
fn store_get_by_label(store_id: String, label: String) -> Result<Option<String>, String>;

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

/// Parse initial_state. Grammar:
///   "[WRITE:]addr1,addr2,..."
/// Default (no WRITE: prefix) is a DRY RUN — nothing is written. Prefix the
/// whole string with "WRITE:" to actually repoint the label. Whitespace around
/// each address is trimmed; empty entries are ignored.
fn parse(state: Value) -> Result<(bool, Vec<String>), String> {
    let raw = match state {
        Value::String(s) => s,
        _ => return Err(String::from(
            "bindings-add: expected initial_state = string \"[WRITE:]addr1,addr2,...\"",
        )),
    };
    let (write, list) = match raw.strip_prefix("WRITE:") {
        Some(rest) => (true, rest),
        None => (false, raw.as_str()),
    };
    let addrs: Vec<String> = list
        .split(',')
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(String::from)
        .collect();
    Ok((write, addrs))
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(ToolState, ()), String> {
    let (write, to_add) = parse(state)?;
    log(format!(
        "[bindings-add] mode={} add={:?}",
        if write { "WRITE" } else { "DRY-RUN" },
        to_add
    ));
    if to_add.is_empty() {
        return Err(String::from("bindings-add: no addresses to add"));
    }

    // 1) Load the current blob.
    let old_ref = store_get_by_label(STORE_ID.into(), BINDINGS_LABEL.into())
        .map_err(|e| format!("get-by-label failed: {}", e))?;
    let (current_bytes, old_ref_str): (Vec<u8>, String) = match old_ref {
        Some(r) => {
            let bytes = store_get(STORE_ID.into(), r.clone())
                .map_err(|e| format!("get blob failed: {}", e))?;
            (bytes, r)
        }
        None => {
            log("[bindings-add] no existing router-bindings label — starting from empty".into());
            (Vec::new(), String::from("<none>"))
        }
    };
    log(format!(
        "[bindings-add] OLD_REF={} bytes={}",
        old_ref_str,
        current_bytes.len()
    ));

    // 2) Decode.
    let mut bindings: Vec<Binding> = if current_bytes.is_empty() {
        Vec::new()
    } else {
        let value = decode(&current_bytes).map_err(|e| format!("decode failed: {:?}", e))?;
        Vec::<Binding>::try_from(value).map_err(|_| String::from("blob is not a Vec<Binding>"))?
    };
    log(format!("[bindings-add] decoded {} existing bindings", bindings.len()));
    for b in &bindings {
        log(format!("[bindings-add]   existing: {} -> \"{}\"", b.address, b.mailbox_id));
    }

    // 3) ROUND-TRIP SELF-CHECK: re-encode the decoded list unchanged and confirm
    //    it reproduces the stored bytes exactly. If not, our encoder does not
    //    match the stored format — ABORT rather than risk corrupting the blob.
    if !current_bytes.is_empty() {
        let reencoded_value: Value = bindings.clone().into();
        let reencoded = encode(&reencoded_value).map_err(|e| format!("re-encode failed: {:?}", e))?;
        if reencoded == current_bytes {
            log("[bindings-add] ROUND-TRIP OK: re-encode reproduces the stored blob byte-for-byte".into());
        } else {
            return Err(format!(
                "ROUND-TRIP MISMATCH (encoder does not match stored format): stored={} bytes, re-encoded={} bytes. ABORTING — no write.",
                current_bytes.len(),
                reencoded.len()
            ));
        }
    }

    // 4) Append (idempotent). Empty mailbox_id = "known address, spawn lazily on
    //    first access" — the lazy-spawn router's own init sets every loaded
    //    binding's mailbox_id to empty anyway, so this is the correct marker.
    let mut added = 0usize;
    for addr in &to_add {
        if bindings.iter().any(|b| &b.address == addr) {
            log(format!("[bindings-add] SKIP (already present): {}", addr));
        } else {
            bindings.push(Binding {
                address: addr.clone(),
                mailbox_id: String::new(),
            });
            added += 1;
            log(format!("[bindings-add] ADD: {} -> \"\" (lazy)", addr));
        }
    }
    log(format!(
        "[bindings-add] added={} new_total={}",
        added,
        bindings.len()
    ));

    // 5) Write (only in WRITE mode, and only if something changed).
    if !write {
        log("[bindings-add] DRY-RUN — not writing. Re-run with a \"WRITE:\" prefix to apply.".into());
        return Ok((ToolState { done: true }, ()));
    }
    if added == 0 {
        log("[bindings-add] nothing to add (all present) — not writing.".into());
        return Ok((ToolState { done: true }, ()));
    }
    let new_value: Value = bindings.clone().into();
    let new_bytes = encode(&new_value).map_err(|e| format!("encode failed: {:?}", e))?;
    let new_ref = store_store_at_label(STORE_ID.into(), BINDINGS_LABEL.into(), new_bytes)
        .map_err(|e| format!("store-at-label failed: {}", e))?;
    log(format!(
        "[bindings-add] WROTE router-bindings: OLD_REF={} -> NEW_REF={} ({} addresses). REVERT = repoint the label to OLD_REF.",
        old_ref_str, new_ref, bindings.len()
    ));
    Ok((ToolState { done: true }, ()))
}
