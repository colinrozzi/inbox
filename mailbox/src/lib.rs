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
    }
    exports {
        theater:simple/actor.init: func(state: value, address: string) -> result<mailbox-state, string>,
        theater:inbox/mailbox.list-since: func(state: mailbox-state, cursor: u64) -> result<tuple<mailbox-state, inbox-page>, string>,
        theater:inbox/mailbox.put-message: func(state: mailbox-state, from: string, to: string, subject: string, body: string) -> result<tuple<mailbox-state, u64>, string>,
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

fn label_for(address: &str) -> String {
    let mut s = String::from("mailbox:");
    s.push_str(address);
    s
}

fn load_state(address: &str) -> Option<MailboxState> {
    let label = label_for(address);
    let content_ref = store_get_by_label(STORE_ID.into(), label).ok()??;
    let bytes = store_get(STORE_ID.into(), content_ref).ok()?;
    let value = decode(&bytes).ok()?;
    MailboxState::try_from(value).ok()
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
fn init(_state: Value, address: String) -> Result<(MailboxState, ()), String> {
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
) -> Result<(MailboxState, u64), String> {
    let mut state = state;
    let id = state.messages.len() as u64;
    let msg = Message {
        id,
        from,
        to,
        subject,
        body,
        received_at: 0, // TODO: wire up clock import once theater timer.now() works from pack actors
    };
    state.messages.push(msg);
    log(format!("[inbox-mailbox] stored message id={} for {}", id, state.address));
    save_state(&state);
    Ok((state, id))
}
