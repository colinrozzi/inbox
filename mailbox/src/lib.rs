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

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
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
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
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
        theater:inbox/mailbox.put-message: func(state: mailbox-state, from: string, to: string, subject: string, body: string, message-id: string, in-reply-to: string, references: string) -> result<tuple<mailbox-state, u64>, string>,
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
                    for key in ["message_id", "in_reply_to", "references", "thread_id"] {
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
) -> Result<(MailboxState, u64), String> {
    let mut state = state;
    let id = state.messages.len() as u64;
    let thread_id = derive_thread_id(&message_id, &in_reply_to, &references);
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
    };
    state.messages.push(msg);
    log(format!("[inbox-mailbox] stored message id={} for {}", id, state.address));
    save_state(&state);
    Ok((state, id))
}
