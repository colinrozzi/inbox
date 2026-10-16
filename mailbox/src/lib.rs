//! Inbox mailbox actor: long-lived, one-per-address holder of messages.
//!
//! in-module-state model (packr 0.24 / theater self.pact): exports no longer
//! thread state. A module-global `StateCell<MailboxState>` is the working copy
//! for reads; the `theater:simple/store` is the source of truth. `init`
//! HYDRATES the cell from the store; `put-message` mutates the cell then appends
//! ONE durable record. Store reads/writes are host calls recorded in the chain,
//! so replay hydrates the cell identically.
//!
//! INCREMENTAL STORAGE (this format, supersedes the old whole-blob scheme):
//! each message is its own append entry, so a delivery writes O(1) bytes instead
//! of re-encoding the whole mailbox (the old whole-blob `mailbox:<address>` save
//! was O(n) per delivery => O(n^2) total write amplification, the cold-sync-stall
//! cause). Labels:
//!   - `msg:<address>:<seq>`    -> packr bytes of ONE Message. seq == Message.id
//!                                 (the 0-based contiguous insertion index).
//!   - `mailbox-seq:<address>`  -> packr Value::U64(next_seq); next_seq == count
//!                                 of persisted messages == the seq the NEXT
//!                                 message gets. Its PRESENCE is the
//!                                 "new-format initialized / migration-done" flag.
//!   - `raw:<address>:<id>`     -> content-addressed raw RFC822 (UNCHANGED).
//! `init` reconstructs MailboxState by folding msg:0..next_seq. The old
//! `mailbox:<address>` whole-state blob is now LEGACY: read only by the one-time
//! forward-migration, NEVER written again.
//!
//! !!! ROLLBACK ONE-WAY-DOOR !!!
//! This is a migrate-FORWARD-only data-at-rest format change. Once a mailbox is
//! migrated AND has taken new appends (its seq cursor advanced past the frozen
//! legacy blob), rolling the mailbox actor BACK to the pre-this-change whole-blob
//! code would read the STALE legacy `mailbox:<address>` blob and LOSE every
//! post-migration message. The deploy MUST NOT roll the mailbox actor back after
//! it has taken appends. (Rolling back BEFORE any append is survivable only
//! because the legacy blob is left frozen, never deleted — see `migrate`.)
//!
//! Mailbox uses StateCell DIRECTLY (no #[derive(State)], no actor.get-state
//! export) — the store is the inspectable truth and auto get-state would
//! serialize a multi-MB mailbox through GraphValue on every call.

#![cfg_attr(not(test), no_std)]
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
    /// `raw:<address>:<id>`). Ref only, never inline — inline multi-MB raw bytes
    /// would bloat the per-message `msg:<address>:<seq>` entry and duplicate the
    /// content-addressed blob.
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
        // Read-only usage metering (control-plane polls this for billing): the
        // count of stored messages. No state, no hot-path write — replay-safe.
        theater:inbox/mailbox.message-count: func() -> result<u64, string>,
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

/// LEGACY whole-state label. Read only by the one-time migration (via
/// `load_state`); NEVER written again under the incremental format.
fn label_for(address: &str) -> String {
    let mut s = String::from("mailbox:");
    s.push_str(address);
    s
}

fn raw_label(address: &str, id: u64) -> String {
    format!("raw:{}:{}", address, id)
}

/// Per-message entry label: the packr bytes of ONE Message. `seq` == Message.id.
fn msg_label(address: &str, seq: u64) -> String {
    format!("msg:{}:{}", address, seq)
}

/// Seq-cursor label: packr Value::U64(next_seq). Presence == new-format flag.
fn seq_label(address: &str) -> String {
    format!("mailbox-seq:{}", address)
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

/// Encode `value` and store it at `label`, logging (non-fatal) on encode/store
/// error — the hot-path durability style (a store failure never panics and
/// never drops the in-memory message; same posture as the raw-store path).
fn store_value(label: String, value: Value, ctx: &str) {
    match encode(&value) {
        Ok(bytes) => {
            if let Err(e) = store_store_at_label(STORE_ID.into(), label, bytes) {
                log(format!("[inbox-mailbox] {} store failed: {}", ctx, e));
            }
        }
        Err(e) => log(format!("[inbox-mailbox] {} encode failed: {:?}", ctx, e)),
    }
}

/// Strict variant for the migration: any encode/store error is returned so the
/// caller aborts (migration must be all-or-nothing up to the commit point).
fn store_value_strict(label: String, value: Value) -> Result<(), String> {
    let bytes = encode(&value).map_err(|e| format!("encode failed: {:?}", e))?;
    store_store_at_label(STORE_ID.into(), label, bytes)
        .map(|_| ())
        .map_err(|e| format!("store failed: {}", e))
}

/// Read + decode the seq cursor `mailbox-seq:<address>`.
///   Ok(None)    = cursor ABSENT -> mailbox not yet on the incremental format.
///   Ok(Some(n)) = cursor present; n = next_seq (count of persisted messages).
///   Err(_)      = cursor PRESENT but unreadable -> caller MUST fail loud, same
///                 data-loss-guard class as an unreadable legacy blob (a silent
///                 fall-through to migration here could re-derive a stale state).
fn read_seq_cursor(address: &str) -> Result<Option<u64>, String> {
    let content_ref = match store_get_by_label(STORE_ID.into(), seq_label(address)) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(None),
        Err(e) => return Err(format!("store get-by-label failed: {}", e)),
    };
    let bytes = store_get(STORE_ID.into(), content_ref)
        .map_err(|e| format!("store get failed: {}", e))?;
    let value = decode(&bytes).map_err(|e| format!("seq cursor decode failed: {:?}", e))?;
    match value {
        Value::U64(n) => Ok(Some(n)),
        _ => Err(String::from("seq cursor wrong type (expected u64)")),
    }
}

/// Read + decode ONE per-message entry `msg:<address>:<seq>`.
///   Some(m) = present + decoded.
///   None    = absent (torn tail: a crash mid-append left no entry here) OR the
///             entry is unreadable. Either way the fold stops here; the specific
///             reason is logged. We never panic and never zero-fill a gap.
fn load_message(address: &str, seq: u64) -> Option<Message> {
    let content_ref = match store_get_by_label(STORE_ID.into(), msg_label(address, seq)) {
        Ok(Some(r)) => r,
        Ok(None) => return None, // absent -> torn tail, stop folding
        Err(e) => {
            log(format!("[inbox-mailbox] msg get-by-label failed (seq={}): {}", seq, e));
            return None;
        }
    };
    let bytes = match store_get(STORE_ID.into(), content_ref) {
        Ok(b) => b,
        Err(e) => {
            log(format!("[inbox-mailbox] msg get failed (seq={}): {}", seq, e));
            return None;
        }
    };
    let value = match decode(&bytes) {
        Ok(v) => v,
        Err(e) => {
            log(format!("[inbox-mailbox] msg decode failed (seq={}): {:?}", seq, e));
            return None;
        }
    };
    match Message::try_from(value) {
        Ok(m) => Some(m),
        Err(_) => {
            log(format!("[inbox-mailbox] msg try_from failed (seq={})", seq));
            None
        }
    }
}

/// PURE fold (testable): walk seq `0..next_seq`, pulling each Message from
/// `fetch`. `fetch(seq)` returns `Some(m)` to append-and-continue, or `None` for
/// a torn tail (missing/unreadable entry from a crash mid-append) — on `None` we
/// STOP folding at that seq and keep exactly what we have (never zero-fill).
/// Returns `(messages, torn_at)`: `torn_at = Some(seq)` iff we stopped early.
fn fold_messages<F>(next_seq: u64, mut fetch: F) -> (Vec<Message>, Option<u64>)
where
    F: FnMut(u64) -> Option<Message>,
{
    let mut messages: Vec<Message> = Vec::new();
    let mut seq = 0u64;
    while seq < next_seq {
        match fetch(seq) {
            Some(m) => messages.push(m),
            None => return (messages, Some(seq)),
        }
        seq += 1;
    }
    (messages, None)
}

/// Which hydration path `init` takes, decided PURELY from the two store probes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum InitPath {
    /// New format: seq cursor present. Fold `msg:<address>:0 .. next_seq`.
    Hydrate(u64),
    /// No cursor + legacy whole-blob present -> run the one-time migration.
    Migrate,
    /// No cursor + no legacy either -> a genuinely fresh, empty mailbox.
    Fresh,
    /// No cursor + legacy PRESENT but UNREADABLE -> the data-loss guard: refuse
    /// to start empty (a silent empty would drop persisted mail).
    FailLoud,
}

/// PURE decision (testable). The seq cursor is authoritative: if present we
/// hydrate regardless of any legacy blob. Only with NO cursor do we consult the
/// legacy load result. `Err(_)` (legacy present-but-unreadable) MUST map to
/// `FailLoud`, never `Fresh`.
fn decide_init_path<T, E>(seq_cursor: Option<u64>, legacy: &Result<Option<T>, E>) -> InitPath {
    match seq_cursor {
        Some(next_seq) => InitPath::Hydrate(next_seq),
        None => match legacy {
            Ok(Some(_)) => InitPath::Migrate,
            Ok(None) => InitPath::Fresh,
            Err(_) => InitPath::FailLoud,
        },
    }
}

/// MIGRATION: legacy whole-blob -> incremental. Idempotent + crash-safe.
fn migrate(legacy_state: &MailboxState) -> Result<(), String> {
    // Step 1: write each message as its own per-message entry. Keys are
    // DETERMINISTIC (`msg:<addr>:<id>`), so re-running overwrites identical
    // bytes — an interrupted migration is safe to repeat.
    for m in &legacy_state.messages {
        store_value_strict(msg_label(&legacy_state.address, m.id), m.clone().into())
            .map_err(|e| format!("migration msg write failed (id={}): {}", m.id, e))?;
    }
    // Step 2 = COMMIT POINT: write the seq cursor ONLY AFTER every per-message
    // write above succeeds. Its presence is the "migrated / new-format" signal.
    let n = legacy_state.messages.len() as u64;
    store_value_strict(seq_label(&legacy_state.address), Value::U64(n))
        .map_err(|e| format!("migration seq-cursor write failed: {}", e))?;
    // Step 3: deliberately DO NOT delete the legacy `mailbox:<address>` blob —
    // leave it frozen for rollback/audit; the seq cursor is authoritative once
    // present.
    //
    // CRASH-SAFETY: if the actor dies after step 1 but before step 2, the next
    // boot sees NO seq cursor -> re-enters migration from the (untouched) legacy
    // blob -> re-writes identical msg entries -> reaches step 2. Exactly-once
    // effect; no loss, no dup.
    log(format!(
        "[inbox-mailbox] migrated {}: {} messages -> incremental",
        legacy_state.address, n
    ));
    Ok(())
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
    // HYDRATE: decide by reading the seq cursor FIRST.
    //   cursor present   -> new format, fold msg:<addr>:0..next_seq.
    //   cursor absent    -> consult the LEGACY whole-blob via load_state:
    //                         Ok(Some) -> migrate forward, then use it;
    //                         Ok(None) -> fresh empty mailbox;
    //                         Err(_)   -> legacy present-but-unreadable -> fail loud.
    //   cursor unreadable -> same data-loss guard: fail loud (never start empty).
    let cursor_opt = match read_seq_cursor(&address) {
        Ok(v) => v,
        Err(e) => {
            log(format!(
                "[inbox-mailbox] init ABORT for {}: seq cursor present but unreadable: {} — refusing to start (data-loss guard)",
                address, e
            ));
            return err_str(&format!(
                "mailbox init: seq cursor for {} is unreadable ({}); refusing silent-empty",
                address, e
            ));
        }
    };
    // Only read the legacy whole-blob when there is NO cursor — avoids an extra
    // store read on every steady-state (new-format) boot.
    let legacy: Result<Option<MailboxState>, String> =
        if cursor_opt.is_some() { Ok(None) } else { load_state(&address) };

    // Bind the decision to a local FIRST: this ends the `&legacy` borrow here
    // (a temporary in a `match` scrutinee would otherwise live for the whole
    // match), so the arms below are free to MOVE `legacy`.
    let path = decide_init_path(cursor_opt, &legacy);
    let state = match path {
        InitPath::Hydrate(next_seq) => {
            // Fold the per-message entries back into a MailboxState. A missing
            // entry inside the range is a torn tail (crash mid-append): log
            // loudly and stop — do NOT panic, do NOT zero-fill.
            let (messages, torn) =
                fold_messages(next_seq, |seq| load_message(&address, seq));
            if let Some(at) = torn {
                log(format!(
                    "[inbox-mailbox] TORN TAIL for {}: msg entry seq={} missing/unreadable while cursor={} — crash mid-append; folded first {} message(s) and stopped (NOT zero-filling)",
                    address, at, next_seq, messages.len()
                ));
            }
            MailboxState { address: address.clone(), messages }
        }
        // Genuinely no stored state yet -> a fresh empty mailbox is correct.
        InitPath::Fresh => MailboxState { address: address.clone(), messages: Vec::new() },
        InitPath::Migrate => {
            let legacy_state = match legacy {
                Ok(Some(s)) => s,
                // decide_init_path only yields Migrate on Ok(Some(_)); defensive.
                _ => {
                    log(format!("[inbox-mailbox] init ABORT for {}: internal migrate path without legacy state", address));
                    return err_str("mailbox init: internal error (migrate path without legacy state)");
                }
            };
            if let Err(e) = migrate(&legacy_state) {
                log(format!(
                    "[inbox-mailbox] init ABORT for {}: migration failed: {} — refusing to start (data-loss guard)",
                    address, e
                ));
                return err_str(&format!("mailbox init: migration failed for {}: {}", address, e));
            }
            legacy_state
        }
        // Stored data PRESENT but unreadable -> refuse to start empty (that would
        // silently drop persisted mail). Fail loudly; it's visible + fixable (run
        // the v2->v3 store migration), not a silent wipe. PRESERVED verbatim.
        InitPath::FailLoud => {
            let e = match legacy { Err(e) => e, _ => String::from("unreadable") };
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

/// Read-only usage metering for the control-plane: how many messages this
/// mailbox holds (the messages Vec length). No state, no mutation — a cheap,
/// replay-safe polled read for per-tenant billing.
#[export(name = "theater:inbox/mailbox.message-count")]
fn message_count() -> Value {
    let count = MAILBOX.with(|s| s.messages.len() as u64);
    ok_result(Value::U64(count))
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

    let seq = id; // Message.id IS the sequence number.
    let message = Message {
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
    // Encode for the durable record before handing the struct to the cell.
    let msg_value: Value = message.clone().into();
    MAILBOX.with_mut(|s| s.messages.push(message));

    // Incremental persist (replaces the old whole-state save — the O(n^2) fix).
    // WRITE ORDER MATTERS for crash-safety:
    //   (a) write the durable per-message record FIRST
    //   (b) advance the seq cursor LAST
    // Rationale: the cursor only advances once the message bytes are durable. A
    // crash between (a) and (b) leaves an orphan `msg:<addr>:<seq>` entry BEYOND
    // the cursor that boot's fold IGNORES (next_seq still == seq) — the same
    // "lost an in-flight delivery" durability class as the old
    // crash-between-push-and-save, never a corrupt/half state. A failed store
    // write LOGS (non-fatal, like the raw-store path) — delivery is never
    // dropped. NO whole-state save.
    store_value(msg_label(&address, seq), msg_value, &format!("msg persist (seq={})", seq)); // (a)
    store_value(seq_label(&address), Value::U64(seq + 1), "seq cursor"); // (b) advance LAST
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
                        // Incremental: rewrite ONLY this changed per-message
                        // entry (`msg:<addr>:<id>`), not the whole state. The seq
                        // cursor is unchanged — the message COUNT didn't change.
                        let value: Value = s.messages[i].clone().into();
                        store_value(msg_label(&address, id), value, &format!("backfill msg rewrite (seq={})", id));
                    }
                    Err(e) => log(format!("[inbox-mailbox] backfill store failed id={}: {}", id, e)),
                }
            }
            i += 1;
        }
    });
    log(format!("[inbox-mailbox] backfill-raw: {} reconstructed for {}", n, address));
    ok_result(Value::U64(n))
}

#[cfg(test)]
mod tests {
    // The store calls are host calls (not unit-testable in-crate), so we test
    // the PURE logic: the hydration fold and the init-path decision. The store
    // wiring + write-ordering are verified by `nix build` + reviewer reading.
    use super::{decide_init_path, fold_messages, InitPath, MailboxState, Message};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    // Minimal Message with a given id/seq; the fold only cares about ordering.
    fn msg(id: u64) -> Message {
        Message {
            id,
            from: String::new(),
            to: String::new(),
            subject: String::new(),
            body: String::new(),
            received_at: 0,
            message_id: String::new(),
            in_reply_to: String::new(),
            references: String::new(),
            thread_id: String::new(),
            cc: String::new(),
            raw_ref: String::new(),
        }
    }

    fn empty_state() -> MailboxState {
        MailboxState { address: String::new(), messages: Vec::new() }
    }

    // ---- fold_messages ----

    #[test]
    fn fold_empty_yields_empty() {
        let (msgs, torn) = fold_messages(0, |_| None);
        assert!(msgs.is_empty());
        assert_eq!(torn, None);
    }

    #[test]
    fn fold_full_range_in_order() {
        let store: Vec<Message> = vec![msg(0), msg(1), msg(2)];
        let (msgs, torn) = fold_messages(3, |seq| store.get(seq as usize).cloned());
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].id, 0);
        assert_eq!(msgs[1].id, 1);
        assert_eq!(msgs[2].id, 2);
        assert_eq!(torn, None);
    }

    #[test]
    fn fold_torn_tail_stops_at_gap() {
        // Cursor claims 3 entries, but only seq 0 and 1 are present (a crash
        // mid-append left seq 2 missing). We fold the two we have and STOP — no
        // zero-fill, no panic — reporting the torn seq.
        let store: Vec<Message> = vec![msg(0), msg(1)];
        let (msgs, torn) = fold_messages(3, |seq| store.get(seq as usize).cloned());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id, 0);
        assert_eq!(msgs[1].id, 1);
        assert_eq!(torn, Some(2));
    }

    #[test]
    fn fold_gap_in_middle_stops_early() {
        // A hole at seq 1 => we keep only seq 0 and stop there.
        let (msgs, torn) = fold_messages(3, |seq| if seq == 0 { Some(msg(0)) } else { None });
        assert_eq!(msgs.len(), 1);
        assert_eq!(torn, Some(1));
    }

    // ---- decide_init_path (all four branches) ----

    #[test]
    fn init_path_seq_present_hydrates() {
        // Cursor is authoritative — hydrate even if a legacy blob also exists.
        let no_legacy: Result<Option<MailboxState>, String> = Ok(None);
        assert_eq!(decide_init_path(Some(5), &no_legacy), InitPath::Hydrate(5));
        let with_legacy: Result<Option<MailboxState>, String> = Ok(Some(empty_state()));
        assert_eq!(decide_init_path(Some(0), &with_legacy), InitPath::Hydrate(0));
    }

    #[test]
    fn init_path_no_cursor_legacy_some_migrates() {
        let legacy: Result<Option<MailboxState>, String> = Ok(Some(empty_state()));
        assert_eq!(decide_init_path(None, &legacy), InitPath::Migrate);
    }

    #[test]
    fn init_path_no_cursor_legacy_none_is_fresh() {
        let legacy: Result<Option<MailboxState>, String> = Ok(None);
        assert_eq!(decide_init_path(None, &legacy), InitPath::Fresh);
    }

    #[test]
    fn init_path_no_cursor_legacy_err_fails_loud() {
        // THE DATA-LOSS GUARD: legacy present-but-unreadable must map to
        // FailLoud, NEVER Fresh (a silent empty would drop persisted mail).
        let legacy: Result<Option<MailboxState>, String> = Err(String::from("decode failed"));
        assert_eq!(decide_init_path(None, &legacy), InitPath::FailLoud);
        assert_ne!(decide_init_path(None, &legacy), InitPath::Fresh);
    }
}
