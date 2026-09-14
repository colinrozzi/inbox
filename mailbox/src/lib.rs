//! Inbox mailbox actor: long-lived, one-per-address holder of messages.
//!
//! in-module-state model (packr 0.24 / theater self.pact): exports no longer
//! thread state. A module-global `StateCell<MailboxState>` is the working copy;
//! the `theater:simple/store` label `mailbox:<address>` remains the source of
//! truth. `init` HYDRATES the cell from the store; every `put-message` mutates
//! the cell then writes the whole state back. Store reads/writes are host calls
//! recorded in the chain, so replay hydrates the cell identically (one blob
//! read, not a per-message replay). Raw RFC822 is kept as a content-addressed
//! blob (`raw:<address>:<id>`) with only a ref on each Message.
//!
//! Mailbox uses StateCell DIRECTLY (no #[derive(State)], no actor.get-state
//! export) — the store is the inspectable truth and auto get-state would
//! serialize a multi-MB mailbox through GraphValue on every call.

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value, ValueType};
use theater_guest::StateCell;

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";

// `forward_compatible` (packr 0.24): tolerant/field-add decode so appending a
// trailing field stays rollback-safe (an old build ignores extras; a new build
// defaults missing) — this is what lets us add IMAP flags/folders later without
// a store one-way-door, and it supersedes the old hand-written
// migrate_threading_fields (removed). Drop the 0.11-composite `crate=` attr:
// 0.24's GraphValue derive defaults the crate to packr_abi via the direct dep.
#[derive(Clone, GraphValue)]
#[graph(forward_compatible)]
pub struct Message {
    pub id: u64,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub received_at: u64,
    /// RFC 5322 threading headers, persisted so replies chain + reads group.
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    /// Synthetic grouping key: the root Message-ID of the chain.
    pub thread_id: String,
    /// The message's true `Cc` envelope leg (comma-joined) from the DATA headers
    /// — NOT this mailbox's own delivery leg. Lets a cc-d reader see the real
    /// recipient set. Empty for messages stored before this field existed.
    pub cc: String,
    /// Content-addressed ref to the message's RAW RFC822 bytes (store label
    /// `raw:<address>:<id>`). Ref only, never inline (save writes the whole
    /// mailbox; inline multi-MB raws would re-encode every message per delivery).
    /// The raw is what a mail client / IMAP needs. Empty for pre-retention
    /// messages, direct-injects, or if the blob store failed (delivery is never
    /// dropped for a raw-store failure). Fetch via store.get(STORE_ID, raw_ref).
    pub raw_ref: String,
}

// forward_compatible (see Message) — covers MailboxState's own future growth.
#[derive(Clone, GraphValue)]
#[graph(forward_compatible)]
pub struct MailboxState {
    /// The email address this mailbox is for; the store label suffix.
    pub address: String,
    pub messages: Vec<Message>,
}

#[derive(Clone, GraphValue)]
#[graph(forward_compatible)]
pub struct InboxPage {
    pub messages: Vec<Message>,
    pub next_cursor: u64,
}

pack_types! {
    imports {
        theater:simple/self {
            log: func(msg: string),
        }
        theater:simple/store {
            get: func(store-id: string, content-ref: string) -> result<list<u8>, string>,
            get-by-label: func(store-id: string, label: string) -> result<option<string>, string>,
            store-at-label: func(store-id: string, label: string, content: list<u8>) -> result<string, string>,
        }
        theater:simple/timer {
            now: func() -> u64,
        }
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:inbox/mailbox.list-since: func(cursor: u64) -> result<inbox-page, string>,
        theater:inbox/mailbox.put-message: func(from: string, to: string, subject: string, body: string, message-id: string, in-reply-to: string, references: string, cc: string, raw: string) -> result<u64, string>,
        theater:inbox/mailbox.backfill-raw: func() -> result<u64, string>,
    }
}

#[import(module = "theater:simple/self", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/store", name = "get")]
fn store_get(store_id: String, content_ref: String) -> Result<Vec<u8>, String>;

#[import(module = "theater:simple/store", name = "get-by-label")]
fn store_get_by_label(store_id: String, label: String) -> Result<Option<String>, String>;

#[import(module = "theater:simple/store", name = "store-at-label")]
fn store_store_at_label(store_id: String, label: String, content: Vec<u8>) -> Result<String, String>;

#[import(module = "theater:simple/timer", name = "now")]
fn timer_now() -> u64;

/// The mailbox working copy. Single-threaded guest, so a module-global cell is
/// sound. Hydrated in `init`; mutated + persisted in `put-message`/`backfill`.
static MAILBOX: StateCell<MailboxState> = StateCell::new();

// ---- result-Value helpers (identical across all inbox actors) ----
fn ok_unit() -> Value {
    let u = Value::Tuple(vec![]);
    Value::Result { ok_type: u.infer_type(), err_type: ValueType::String, value: Ok(Box::new(u)) }
}
fn ok_result(v: Value) -> Value {
    Value::Result { ok_type: v.infer_type(), err_type: ValueType::String, value: Ok(Box::new(v)) }
}
fn err_str(msg: &str) -> Value {
    // ok_type is a phantom on the Err branch; unit is the safe placeholder.
    let e = Value::String(String::from(msg));
    Value::Result { ok_type: Value::Tuple(vec![]).infer_type(), err_type: ValueType::String, value: Err(Box::new(e)) }
}

fn label_for(address: &str) -> String {
    let mut s = String::from("mailbox:");
    s.push_str(address);
    s
}

fn raw_label(address: &str, id: u64) -> String {
    format!("raw:{}:{}", address, id)
}

/// Hydrate a MailboxState from the store. None if unset/undecodable. Relies on
/// `forward_compatible` for field-add tolerance (old blobs decode with missing
/// trailing fields defaulted) — no hand-written field migration needed.
// Ok(None)      = no stored state (fresh mailbox; empty is correct).
// Ok(Some(_))   = state present + decoded.
// Err(_)        = state PRESENT but unreadable (old-format / corruption). The
//                 caller MUST hard-fail loudly, NOT fall back to empty — a silent
//                 empty here would drop persisted mail (the v2->v3 store-format
//                 trap: 0.24 decode cleanly Errs on v2 data). Fail-loud so a
//                 missed/pending store migration is visible, never a silent wipe.
fn load_state(address: &str) -> Result<Option<MailboxState>, String> {
    let content_ref = match store_get_by_label(STORE_ID.into(), label_for(address)) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(None),
        Err(e) => return Err(format!("store get-by-label failed: {}", e)),
    };
    let bytes = store_get(STORE_ID.into(), content_ref)
        .map_err(|e| format!("store get failed: {}", e))?;
    let value = decode(&bytes)
        .map_err(|e| format!("decode failed (old-format store data? needs v2->v3 migration): {:?}", e))?;
    let state = MailboxState::try_from(value)
        .map_err(|_| String::from("MailboxState try_from failed (schema mismatch or old-format)"))?;
    Ok(Some(state))
}

fn save_state(state: &MailboxState) {
    let value: Value = state.clone().into();
    match encode(&value) {
        Ok(bytes) => {
            if let Err(e) = store_store_at_label(STORE_ID.into(), label_for(&state.address), bytes) {
                log(format!("[inbox-mailbox] persist failed: {}", e));
            }
        }
        Err(e) => log(format!("[inbox-mailbox] encode failed: {:?}", e)),
    }
}

/// Root Message-ID of the chain: first References id, else In-Reply-To, else own.
fn derive_thread_id(message_id: &str, in_reply_to: &str, references: &str) -> String {
    fn strip(s: &str) -> String {
        s.trim().trim_start_matches('<').trim_end_matches('>').into()
    }
    if let Some(root) = references.split_whitespace().next() {
        return strip(root);
    }
    if !in_reply_to.trim().is_empty() {
        return strip(in_reply_to);
    }
    strip(message_id)
}

/// Synthesize a plain-text RFC822 from a Message's parsed fields, for
/// backfilling pre-retention messages. LOSSY (original attachments/HTML were
/// discarded at receive time); marked X-Inbox-Reconstructed: 1.
fn reconstruct_raw(m: &Message) -> String {
    fn header(out: &mut String, name: &str, value: &str) {
        if !value.trim().is_empty() {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value.trim());
            out.push('\n');
        }
    }
    let mut s = String::new();
    header(&mut s, "From", &m.from);
    header(&mut s, "To", &m.to);
    header(&mut s, "Cc", &m.cc);
    header(&mut s, "Subject", &m.subject);
    header(&mut s, "Message-ID", &m.message_id);
    header(&mut s, "In-Reply-To", &m.in_reply_to);
    header(&mut s, "References", &m.references);
    s.push_str("X-Inbox-Reconstructed: 1\n");
    s.push('\n');
    s.push_str(&m.body);
    s
}

#[export(name = "theater:simple/actor.init")]
fn init(config: Value) -> Value {
    let address = match config {
        Value::String(s) => s,
        _ => return err_str("mailbox init: expected init_state = string (email address)"),
    };
    let state = match load_state(&address) {
        Ok(Some(s)) => s,
        // Genuinely no stored state yet -> a fresh empty mailbox is correct.
        Ok(None) => MailboxState { address: address.clone(), messages: Vec::new() },
        // Stored data PRESENT but unreadable -> refuse to start empty (that would
        // silently drop persisted mail). Fail loudly; it's visible + fixable (run
        // the v2->v3 store migration), not a silent wipe.
        Err(e) => {
            log(format!(
                "[inbox-mailbox] init ABORT for {}: {} — refusing to start empty over present-but-unreadable store data (data-loss guard)",
                address, e
            ));
            return err_str(&format!(
                "mailbox init: present store data for {} is unreadable ({}); refusing silent-empty — migrate v2->v3",
                address, e
            ));
        }
    };
    log(format!("[inbox-mailbox] init {} ({} messages)", address, state.messages.len()));
    MAILBOX.set(state);
    ok_unit()
}

#[export(name = "theater:inbox/mailbox.list-since")]
fn list_since(cursor: u64) -> Value {
    let page = MAILBOX.with(|state| {
        let messages: Vec<Message> = state
            .messages
            .iter()
            .filter(|m| m.id >= cursor)
            .cloned()
            .collect();
        let next_cursor = state.messages.last().map(|m| m.id + 1).unwrap_or(cursor);
        InboxPage { messages, next_cursor }
    });
    ok_result(page.into())
}

#[export(name = "theater:inbox/mailbox.put-message")]
#[allow(clippy::too_many_arguments)]
fn put_message(
    from: String,
    to: String,
    subject: String,
    body: String,
    message_id: String,
    in_reply_to: String,
    references: String,
    cc: String,
    raw: String,
) -> Value {
    let address = MAILBOX.with(|s| s.address.clone());
    let id = MAILBOX.with(|s| s.messages.len() as u64);
    let thread_id = derive_thread_id(&message_id, &in_reply_to, &references);

    // Persist the raw RFC822 as a content-addressed blob; keep only the ref. A
    // store failure logs and falls back to empty — never drops the message.
    let raw_ref = if raw.is_empty() {
        String::new()
    } else {
        match store_store_at_label(STORE_ID.into(), raw_label(&address, id), raw.into_bytes()) {
            Ok(r) => r,
            Err(e) => {
                log(format!("[inbox-mailbox] raw store failed (id={}): {}", id, e));
                String::new()
            }
        }
    };

    MAILBOX.with_mut(|s| {
        s.messages.push(Message {
            id,
            from,
            to,
            subject,
            body,
            received_at: timer_now(),
            message_id,
            in_reply_to,
            references,
            thread_id,
            cc,
            raw_ref,
        });
    });
    MAILBOX.with(|s| save_state(s));
    log(format!("[inbox-mailbox] stored message id={} for {}", id, address));
    ok_result(Value::U64(id))
}

/// One-shot, idempotent: give every message a raw_ref by reconstructing one from
/// its parsed fields (see reconstruct_raw). Skips messages that already have a
/// ref, so it's safe to re-run. Returns how many were filled this call.
#[export(name = "theater:inbox/mailbox.backfill-raw")]
fn backfill_raw() -> Value {
    let address = MAILBOX.with(|s| s.address.clone());
    let mut n: u64 = 0;
    MAILBOX.with_mut(|s| {
        let mut i = 0;
        while i < s.messages.len() {
            if s.messages[i].raw_ref.is_empty() {
                let raw = reconstruct_raw(&s.messages[i]);
                let id = s.messages[i].id;
                match store_store_at_label(STORE_ID.into(), raw_label(&address, id), raw.into_bytes()) {
                    Ok(r) => {
                        s.messages[i].raw_ref = r;
                        n += 1;
                    }
                    Err(e) => log(format!("[inbox-mailbox] backfill store failed id={}: {}", id, e)),
                }
            }
            i += 1;
        }
    });
    if n > 0 {
        MAILBOX.with(|s| save_state(s));
    }
    log(format!("[inbox-mailbox] backfill-raw: {} reconstructed for {}", n, address));
    ok_result(Value::U64(n))
}
