//! Decode benchmark: reproduces the inbox 0.10.2 mail-spine hang in ONE actor,
//! with fully SYNTHETIC mail (no real data).
//!
//! The prod hang is the mailbox actor's `load_state` decoding its persisted
//! `MailboxState` (a `Vec<Message>` of hundreds of records) via
//! `packr_guest::decode` on restart. This actor reproduces exactly that decode:
//! build a MailboxState of N synthetic messages, `encode` it (as the mailbox's
//! `save_state` does), then `decode` it back (as `load_state` does) — and log
//! around the decode so a hang pins to it. Spawn once per N to get the curve.
//!
//! Message/MailboxState are byte-identical GraphValue types to the real mailbox,
//! so the decoded Value node structure (and the decoder path) is the same.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value};

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
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub thread_id: String,
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct MailboxState {
    pub address: String,
    pub messages: Vec<Message>,
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct BenchState {
    pub done: bool,
}

pack_types! {
    imports {
        theater:simple/runtime {
            log: func(msg: string),
        }
        theater:simple/timer {
            now: func() -> u64,
        }
    }
    exports {
        theater:simple/actor.init: func(state: value) -> result<bench-state, string>,
    }
}

#[import(module = "theater:simple/runtime", name = "log")]
fn log(msg: String);

#[import(module = "theater:simple/timer", name = "now")]
fn timer_now() -> u64;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// ~910-char base64-ish body (14 lines x 64 chars), varied per message so lines
/// are distinct (like real base64 attachment/DKIM content), not a collapsible run.
fn make_body(i: u64) -> String {
    let mut s = String::with_capacity(920);
    for line in 0u64..14 {
        for c in 0u64..64 {
            let v = (i.wrapping_mul(131).wrapping_add(line.wrapping_mul(64).wrapping_add(c)).wrapping_mul(31)) % 64;
            s.push(B64[v as usize] as char);
        }
        s.push('\n');
    }
    s
}

fn make_message(i: u64) -> Message {
    Message {
        id: i,
        from: format!("sender{}@example.com", i % 37),
        to: String::from("inbox-dev@colinrozzi.com"),
        subject: format!("Synthetic benchmark message number {}", i),
        body: make_body(i),
        received_at: 1_700_000_000 + i,
        message_id: format!("<bench-{}@example.com>", i),
        in_reply_to: String::new(),
        references: String::new(),
        thread_id: format!("<thread-{}@example.com>", i),
    }
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(BenchState, ()), String> {
    let n: u64 = match state {
        Value::String(s) => s.trim().parse().unwrap_or(100),
        _ => 100,
    };
    log(format!("bench: building MailboxState with N={} messages", n));

    let mut messages: Vec<Message> = Vec::new();
    for i in 0..n {
        messages.push(make_message(i));
    }
    let mb = MailboxState {
        address: String::from("inbox-dev@colinrozzi.com"),
        messages,
    };

    // encode (as mailbox save_state does)
    let value: Value = mb.into();
    let t_enc0 = timer_now();
    let bytes = encode(&value).map_err(|e| format!("encode failed: {:?}", e))?;
    let t_enc1 = timer_now();
    log(format!(
        "bench: N={} encoded {} bytes in {}ms; now DECODING (the load_state hang site)...",
        n,
        bytes.len(),
        t_enc1.saturating_sub(t_enc0)
    ));

    // decode (as mailbox load_state does) — THE suspect quadratic
    let t_dec0 = timer_now();
    let decoded = decode(&bytes).map_err(|e| format!("decode failed: {:?}", e))?;
    let t_dec1 = timer_now();
    let mb2 = MailboxState::try_from(decoded).map_err(|_| String::from("try_from failed"))?;

    log(format!(
        "BENCH_RESULT N={} bytes={} encode_ms={} decode_ms={} messages={}",
        n,
        bytes.len(),
        t_enc1.saturating_sub(t_enc0),
        t_dec1.saturating_sub(t_dec0),
        mb2.messages.len()
    ));
    Ok((BenchState { done: true }, ()))
}
