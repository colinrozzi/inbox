//! Inbox mailbox actor: long-lived, one-per-address holder of messages.
//!
//! State (messages) is persisted to `theater:simple/store` under the label
//! `mailbox:<address>`. On init the actor restores from that label if
//! present; on every `put-message` it writes the whole new state back.
//! The address is passed in at init time by the router so the actor knows
//! which label to use.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";

// `forward_compatible`: make record decode tolerant of field-set changes so a
// store field-add is ROLLBACK-SAFE. A NAMED record decodes by name and the attr
// (a) ignores EXTRA fields (an old build reads new data — the one-way-door fix)
// and (b) DEFAULTS missing fields (a new build reads old data — this supersedes
// the hand-written migrate_threading_fields). Encode is unchanged (no wire
// change). MUST be live BEFORE the next field is appended (see deploy sequence:
// this attr ships first, schema-neutral; the cc-add follows, rollback-safe).
// NOTE separate #[graph(...)] attrs, NOT combined: on 0.12.5 the combined form
// `#[graph(crate = "...", forward_compatible)]` mis-parses the crate arg and
// silently defaults it to packr_abi (wrong ABI for a composite_abi guest). The
// derive scans all attrs, so two lines is correct. (0.12.6 fixes the combined
// form; adopt it later — pure UX.)
#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
#[graph(forward_compatible)]
pub struct Message {
    pub id: u64,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub received_at: u64,
    /// RFC 5322 threading headers, persisted so replies can chain and reads
    /// can eventually group. Empty for messages stored before the threading
    /// migration (see `migrate_threading_fields`) or injected without them.
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    /// Synthetic grouping key: the root Message-ID of the chain. Derived at
    /// store time (see `derive_thread_id`); the read-side grouping view is a
    /// follow-up.
    pub thread_id: String,
    /// The message's true `Cc` envelope leg (comma-joined), parsed from the DATA
    /// headers — NOT this mailbox's own delivery leg. With `to` (the true `To`)
    /// it lets a cc-d reader see the real recipient set and know they were cc-d,
    /// not mis-addressed. Added AFTER thread_id (trailing) so old messages
    /// migrate by padding a single trailing field (see `migrate_threading_fields`).
    /// Empty for messages stored before this field existed.
    pub cc: String,
    /// Content-addressed ref to the message's RAW RFC822 bytes in the store
    /// (stored under label `raw:<address>:<id>`). We keep only the ref here, NOT
    /// the raw inline, because `save_state` rewrites the WHOLE mailbox on every
    /// new message — inlining multi-MB raws would re-encode every message's body
    /// on each delivery. The raw is what a mail client / IMAP needs (full
    /// headers, MIME parts, attachments, HTML) that the parsed `subject`/`body`
    /// above discard. Empty for messages stored before this field existed, or
    /// injected without a raw form (e.g. direct-inject), or if the blob store
    /// failed (logged; delivery is never dropped for a raw-store failure).
    /// Fetch with `store.get(STORE_ID, raw_ref)`.
    pub raw_ref: String,
}

// forward_compatible (see Message) — covers MailboxState's own future growth.
// Separate attrs, not combined (see Message note).
#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
#[graph(forward_compatible)]
pub struct MailboxState {
    /// The email address this mailbox is for; used as the store label suffix.
    pub address: String,
    pub messages: Vec<Message>,
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct InboxPage {
    pub messages: Vec<Message>,
    pub next_cursor: u64,
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
        theater:simple/timer {
            now: func() -> u64,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<mailbox-state, string>,
        theater:inbox/mailbox.list-since: func(state: mailbox-state, cursor: u64) -> result<tuple<mailbox-state, inbox-page>, string>,
        theater:inbox/mailbox.put-message: func(state: mailbox-state, from: string, to: string, subject: string, body: string, message-id: string, in-reply-to: string, references: string, cc: string, raw: string) -> result<tuple<mailbox-state, u64>, string>,
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

#[import(module = "theater:simple/timer", name = "now")]
fn timer_now() -> u64;

fn label_for(address: &str) -> String {
    let mut s = String::from("mailbox:");
    s.push_str(address);
    s
}

fn load_state(address: &str) -> Option<MailboxState> {
    let label = label_for(address);
    let content_ref = store_get_by_label(STORE_ID.into(), label).ok()??;
    let bytes = store_get(STORE_ID.into(), content_ref).ok()?;
    let mut value = decode(&bytes).ok()?;
    migrate_threading_fields(&mut value);
    MailboxState::try_from(value).ok()
}

/// Backfill threading fields onto message records persisted before threading
/// existed. The `GraphValue` derive for `Message` is strict — it rejects a
/// record whose field set doesn't match exactly (wrong count / missing name).
/// A pre-migration record has six fields; without this patch `try_from` would
/// fail, `load_state` would return `None`, and the mailbox would silently
/// re-init empty — orphaning the whole history on the next write. We add any
/// missing threading field as an empty string so old mailboxes load intact.
fn migrate_threading_fields(value: &mut Value) {
    let fields = match value {
        Value::Record { fields, .. } => fields,
        _ => return,
    };
    for (name, v) in fields.iter_mut() {
        if name.as_str() != "messages" {
            continue;
        }
        if let Value::List { items, .. } = v {
            for item in items.iter_mut() {
                if let Value::Record { fields: mf, .. } = item {
                    for key in ["message_id", "in_reply_to", "references", "thread_id", "cc", "raw_ref"] {
                        if !mf.iter().any(|(n, _)| n.as_str() == key) {
                            mf.push((String::from(key), Value::String(String::new())));
                        }
                    }
                }
            }
        }
    }
}

/// The thread's grouping key is the root Message-ID of the chain: the first
/// id in `References` (the original), else the `In-Reply-To` target, else this
/// message's own Message-ID (it starts a fresh thread). Angle brackets are
/// normalized off so the key is stable regardless of header formatting.
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

fn save_state(state: &MailboxState) {
    let label = label_for(&state.address);
    let value: Value = state.clone().into();
    match encode(&value) {
        Ok(bytes) => {
            if let Err(e) = store_store_at_label(STORE_ID.into(), label, bytes) {
                log(format!("[inbox-mailbox] persist failed: {}", e));
            }
        }
        Err(e) => log(format!("[inbox-mailbox] encode failed: {:?}", e)),
    }
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(MailboxState, ()), String> {
    let address = match state {
        Value::String(s) => s,
        _ => return Err(String::from(
            "mailbox init: expected init_state = string (email address)",
        )),
    };
    let state = load_state(&address).unwrap_or_else(|| MailboxState {
        address: address.clone(),
        messages: Vec::new(),
    });
    log(format!(
        "[inbox-mailbox] init {} ({} messages)",
        address,
        state.messages.len()
    ));
    Ok((state, ()))
}

#[export(name = "theater:inbox/mailbox.list-since")]
fn list_since(
    state: MailboxState,
    cursor: u64,
) -> Result<(MailboxState, InboxPage), String> {
    let messages: Vec<Message> = state
        .messages
        .iter()
        .filter(|m| m.id >= cursor)
        .cloned()
        .collect();
    let next_cursor = state.messages.last().map(|m| m.id + 1).unwrap_or(cursor);
    Ok((state, InboxPage { messages, next_cursor }))
}

#[export(name = "theater:inbox/mailbox.put-message")]
fn put_message(
    state: MailboxState,
    from: String,
    to: String,
    subject: String,
    body: String,
    message_id: String,
    in_reply_to: String,
    references: String,
    cc: String,
    raw: String,
) -> Result<(MailboxState, u64), String> {
    let mut state = state;
    let id = state.messages.len() as u64;
    let thread_id = derive_thread_id(&message_id, &in_reply_to, &references);
    // Persist the raw RFC822 as a content-addressed blob; keep only the ref on
    // the Message (see Message::raw_ref). A store failure must NOT drop the
    // message — log and fall back to an empty ref so the parsed fields still land.
    let raw_ref = if raw.is_empty() {
        String::new()
    } else {
        let label = format!("raw:{}:{}", state.address, id);
        match store_store_at_label(STORE_ID.into(), label, raw.into_bytes()) {
            Ok(r) => r,
            Err(e) => {
                log(format!("[inbox-mailbox] raw store failed (id={}): {}", id, e));
                String::new()
            }
        }
    };
    let msg = Message {
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
    };
    state.messages.push(msg);
    log(format!("[inbox-mailbox] stored message id={} for {}", id, state.address));
    save_state(&state);
    Ok((state, id))
}
