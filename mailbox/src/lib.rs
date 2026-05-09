//! Inbox mailbox actor: long-lived singleton holding messages.
//!
//! For the MVP this is a single inbox. Multi-tenancy comes later.
//!
//! State is the message log. `id` is just the index into the log so
//! `since=<id>` cursor reads are trivial.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

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
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<mailbox-state, string>,
        theater:inbox/mailbox.list-since: func(state: mailbox-state, cursor: u64) -> result<tuple<mailbox-state, inbox-page>, string>,
        theater:inbox/mailbox.put-message: func(state: mailbox-state, from: string, to: string, subject: string, body: string) -> result<tuple<mailbox-state, u64>, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[export(name = "theater:simple/actor.init")]
fn init(_state: Value) -> Result<(MailboxState, ()), String> {
    log(String::from("[inbox-mailbox] init"));
    Ok((MailboxState { messages: Vec::new() }, ()))
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
        received_at: 0, // TODO: wire up clock import
    };
    log(format!("[inbox-mailbox] stored message id={}", id));
    state.messages.push(msg);
    Ok((state, id))
}
