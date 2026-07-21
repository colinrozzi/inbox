//! Router finalization bench: reproduces the post-load 98% spin in ONE actor,
//! no real data. The manager confirmed all mailboxes load fine on 0.10.4, then
//! the acceptor never binds — the spin is in the router's save_bindings /
//! RouterState finalization (encode). This exercises exactly that:
//! build a RouterState of N Bindings (byte-identical types to the real router),
//! then run the two encode paths save_bindings does, plus the RouterState return
//! encode, with in-guest timing. If one loops/blows up, it's pinned to that
//! encode call — and my mailbox bench (which encoded a 1MB MailboxState fine)
//! won't have caught it because it's Binding/RouterState-type-specific.

#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value};

packr_guest::setup_guest!();

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct Binding {
    pub address: String,
    pub mailbox_id: String,
}

#[derive(Clone, GraphValue)]
#[graph(crate = "packr_guest::composite_abi")]
pub struct RouterState {
    pub mailbox_manifest: String,
    pub bindings: Vec<Binding>,
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

// Realistic bindings: addresses share the @colinrozzi.com suffix (like prod's
// 17), mailbox_ids are UUID-shaped and share a fixed length/charset.
fn make_binding(i: u64) -> Binding {
    Binding {
        address: format!("agent-{}-dev@colinrozzi.com", i),
        mailbox_id: format!("{:08x}-5da8-4a0a-940d-f510be0582de", i.wrapping_mul(2654435761)),
    }
}

#[export(name = "theater:simple/actor.init")]
fn init(state: Value) -> Result<(BenchState, ()), String> {
    let n: u64 = match state {
        Value::String(s) => s.trim().parse().unwrap_or(17),
        _ => 17,
    };
    log(format!("router-bench: building RouterState with N={} bindings", n));

    let mut bindings: Vec<Binding> = Vec::new();
    for i in 0..n {
        bindings.push(make_binding(i));
    }
    let rs = RouterState {
        mailbox_manifest: String::from("/var/lib/inbox/manifests/mailbox.toml"),
        bindings: bindings.clone(),
    };

    // Path A — save_bindings: Vec<Binding> -> Value -> encode (mailbox-router/lib.rs:102)
    log(format!("router-bench: N={} PATH A save_bindings: Vec<Binding> -> Value ...", n));
    let t0 = timer_now();
    let bv: Value = bindings.clone().into();
    let t1 = timer_now();
    log(format!("router-bench: N={} ... into() {}ms; encode ...", n, t1.saturating_sub(t0)));
    let bbytes = encode(&bv).map_err(|e| format!("bindings encode failed: {:?}", e))?;
    let t2 = timer_now();
    log(format!(
        "router-bench: N={} PATH A OK: into={}ms encode={}ms bytes={}",
        n, t1.saturating_sub(t0), t2.saturating_sub(t1), bbytes.len()
    ));

    // Path B — RouterState return: RouterState -> Value -> encode (what theater
    // encodes when router.init returns Ok(RouterState)).
    log(format!("router-bench: N={} PATH B RouterState -> Value -> encode ...", n));
    let t3 = timer_now();
    let rv: Value = rs.into();
    let rbytes = encode(&rv).map_err(|e| format!("routerstate encode failed: {:?}", e))?;
    let t4 = timer_now();
    log(format!(
        "router-bench: N={} PATH B OK: encode={}ms bytes={}",
        n, t4.saturating_sub(t3), rbytes.len()
    ));

    // Round-trip decode too (defensive; the init-arg / re-decode path).
    let _ = decode(&bbytes).map_err(|e| format!("bindings decode failed: {:?}", e))?;

    log(format!("ROUTER_BENCH_RESULT N={} bindings_encode_bytes={} routerstate_encode_bytes={} ALL OK", n, bbytes.len(), rbytes.len()));
    Ok((BenchState { done: true }, ()))
}
