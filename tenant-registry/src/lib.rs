//! Tenant registry: the identity + capability oracle for multi-tenant agent mail.
//!
//! Long-lived singleton. Owns TENANTS and their KEYS — nothing about mailboxes
//! (the mailbox→tenant fact lives in the mailbox-router's Binding). The API path
//! (api-handler) asks `resolve` on every request to turn a bearer token into a
//! tenant + capability set; the control-plane exports (create-tenant / mint-key /
//! revoke-key / list-keys) manage keys.
//!
//! MODEL (see the tenancy design doc): a tenant = a durable identity + a single
//! permission set. Keys are equal on the DATA plane (all reach the tenant's
//! mailboxes); the only per-key axis is the capability set, a FIXED vocabulary of
//! `use` (access mailboxes) and `admin` (mint/revoke this tenant's keys, create
//! its mailboxes). admin keys may mint admin keys; an admin only ever mints within
//! its OWN tenant — enforced by api-handler passing the caller's own tenant_id.
//!
//! REVOCATION is FLAT: revoke removes exactly that key, no cascade. Recovery from a
//! compromised admin = revoke the admin keys first (freezes minting), then the rest.
//!
//! TOKENS: never stored in the clear — we keep `sha256(token)` and compare on
//! resolve (D8). Generation is deterministic (in-guest randomness would break
//! replay): `token = hmac_sha256(seed, tenant_id:seq)`, where `seed` is a secret
//! seeded into the store out-of-band (label `tenant-registry-seed`, same pattern as
//! bearer/dkim). Unguessable without the seed, a pure function of persisted state.
//!
//! in-module-state model (packr 0.24): RegistryState lives in a `#[derive(State)]`
//! cell; exports no longer thread state. State persists under the store label
//! `tenant-registry`; init hydrates the cell, every mutation write-through-persists.

#![no_std]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use packr_guest::{decode, encode, export, import, pack_types, GraphValue, Value, ValueType};
use sha2::{Digest, Sha256};
use theater_guest::State;

packr_guest::setup_guest!();

const STORE_ID: &str = "inbox";
const STATE_LABEL: &str = "tenant-registry";
const SEED_LABEL: &str = "tenant-registry-seed";

const CAP_USE: &str = "use";
const CAP_ADMIN: &str = "admin";

// ---- persisted types (forward_compatible: field-add tolerant, rollback-safe) ----

#[derive(Clone, GraphValue)]
#[graph(forward_compatible)]
pub struct Key {
    /// Stable key id (for revocation / listing). NOT the token.
    pub id: String,
    /// sha256(token) hex — the token itself is never stored.
    pub token_hash: String,
    /// Capability set, a subset of {use, admin}.
    pub caps: Vec<String>,
    /// Human label ("root", "agent-x").
    pub label: String,
    /// Monotonic creation sequence — ordering + audit WITHOUT a wall clock
    /// (a guest actor is a deterministic replayable projection; no clock).
    pub created_seq: u64,
    /// Key id that minted this one ("" for an operator-created root). Audit note
    /// only — revocation is flat, so this is NOT a lifecycle link.
    pub created_by: String,
}

#[derive(Clone, GraphValue)]
#[graph(forward_compatible)]
pub struct Tenant {
    pub id: String,
    pub label: String,
    pub keys: Vec<Key>,
}

#[derive(Clone, GraphValue, State)]
pub struct RegistryState {
    pub tenants: Vec<Tenant>,
    /// Monotonic counter for tenant ids, key ids, and created_seq.
    pub next_seq: u64,
}

// ---- return DTOs (map to the pact record types referenced in exports) ----

#[derive(Clone, GraphValue)]
pub struct Resolved {
    pub tenant_id: String,
    pub key_id: String,
    pub caps: Vec<String>,
}

#[derive(Clone, GraphValue)]
pub struct KeyMeta {
    pub key_id: String,
    pub label: String,
    pub caps: Vec<String>,
    pub created_seq: u64,
    pub created_by: String,
}

#[derive(Clone, GraphValue)]
pub struct Minted {
    pub key_id: String,
    pub token: String,
}

#[derive(Clone, GraphValue)]
pub struct TenantCreated {
    pub tenant_id: String,
    pub root_token: String,
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
    }
    exports {
        theater:simple/actor.init: func(config: value) -> result<_, string>,
        theater:simple/actor.get-state: func() -> value,
        theater:inbox/registry.resolve: func(token: string) -> result<option<resolved>, string>,
        theater:inbox/registry.create-tenant: func(label: string) -> result<tenant-created, string>,
        theater:inbox/registry.mint-key: func(tenant-id: string, caps: list<string>, label: string, created-by: string) -> result<minted, string>,
        theater:inbox/registry.import-key: func(tenant-id: string, token: string, caps: list<string>, label: string) -> result<string, string>,
        theater:inbox/registry.revoke-key: func(key-id: string) -> result<_, string>,
        theater:inbox/registry.list-keys: func(tenant-id: string) -> result<list<key-meta>, string>,
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

// ---- result-Value helpers (identical across all inbox actors) ----
fn ok_unit() -> Value {
    let u = Value::Tuple(vec![]);
    Value::Result { ok_type: u.infer_type(), err_type: ValueType::String, value: Ok(Box::new(u)) }
}
fn ok_result(v: Value) -> Value {
    Value::Result { ok_type: v.infer_type(), err_type: ValueType::String, value: Ok(Box::new(v)) }
}
fn err_str(msg: &str) -> Value {
    let e = Value::String(String::from(msg));
    Value::Result { ok_type: Value::Tuple(vec![]).infer_type(), err_type: ValueType::String, value: Err(Box::new(e)) }
}

// ---- crypto: sha256 hex + hand-rolled HMAC-SHA256 (avoids a new crate dep) ----
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_digit(b >> 4));
        s.push(hex_digit(b & 0x0f));
    }
    s
}
fn hex_digit(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + (n - 10)) as char
    }
}
fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    to_hex(&h.finalize())
}
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(key);
        k[..32].copy_from_slice(&h.finalize());
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = {
        let mut h = Sha256::new();
        h.update(ipad);
        h.update(msg);
        h.finalize()
    };
    let mut h = Sha256::new();
    h.update(opad);
    h.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// Read the token-generation seed from the store (seeded out-of-band, like
/// bearer/dkim). Only needed to MINT (create-tenant / mint-key); resolve doesn't
/// touch it. Absent/empty is a hard error — you cannot mint without it.
fn read_seed() -> Result<Vec<u8>, String> {
    let content_ref = store_get_by_label(STORE_ID.into(), SEED_LABEL.into())
        .map_err(|e| format!("seed get-by-label failed: {}", e))?
        .ok_or_else(|| {
            String::from(
                "tenant-registry-seed absent — the manager must seed it into the store before minting",
            )
        })?;
    let bytes = store_get(STORE_ID.into(), content_ref).map_err(|e| format!("seed get failed: {}", e))?;
    if bytes.is_empty() {
        return Err(String::from("tenant-registry-seed is present but EMPTY"));
    }
    Ok(bytes)
}

// Ok(None)    = no state stored yet (fresh registry; empty is correct).
// Ok(Some(_)) = state present + decoded.
// Err(_)      = state PRESENT but unreadable (old-format / corruption) — caller MUST
//               hard-fail, mirroring the router/mailbox data-loss guard (silently
//               starting empty would orphan every tenant + key).
fn load_state() -> Result<Option<RegistryState>, String> {
    let content_ref = match store_get_by_label(STORE_ID.into(), STATE_LABEL.into()) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(None),
        Err(e) => return Err(format!("store get-by-label failed: {}", e)),
    };
    let bytes = store_get(STORE_ID.into(), content_ref).map_err(|e| format!("store get failed: {}", e))?;
    let value = decode(&bytes).map_err(|e| format!("decode failed (old-format state? needs migration): {:?}", e))?;
    let state = RegistryState::try_from(value).map_err(|_| String::from("state try_from failed (schema mismatch or old-format)"))?;
    Ok(Some(state))
}

fn save_state(state: &RegistryState) {
    let value: Value = state.clone().into();
    match encode(&value) {
        Ok(bytes) => {
            if let Err(e) = store_store_at_label(STORE_ID.into(), STATE_LABEL.into(), bytes) {
                log(format!("[tenant-registry] persist failed: {}", e));
            }
        }
        Err(e) => log(format!("[tenant-registry] encode failed: {:?}", e)),
    }
}

fn caps_valid(caps: &[String]) -> Result<(), String> {
    for c in caps {
        if c != CAP_USE && c != CAP_ADMIN {
            return Err(format!("unknown capability: {}", c));
        }
    }
    Ok(())
}

#[export(name = "theater:simple/actor.init")]
fn init(_config: Value) -> Value {
    log(String::from("[tenant-registry] init"));
    let state = match load_state() {
        Ok(Some(s)) => s,
        Ok(None) => RegistryState { tenants: Vec::new(), next_seq: 1 },
        Err(e) => {
            log(format!(
                "[tenant-registry] init ABORT: {} — refusing to start empty over present-but-unreadable state (would orphan all tenants/keys)",
                e
            ));
            return err_str(&format!(
                "tenant-registry init: present state unreadable ({}); refusing silent-empty — migrate",
                e
            ));
        }
    };
    RegistryState::set(state);
    ok_unit()
}

#[export(name = "theater:inbox/registry.resolve")]
fn resolve(token: String) -> Value {
    let hash = sha256_hex(token.as_bytes());
    let found = RegistryState::with(|s| {
        for t in &s.tenants {
            for k in &t.keys {
                if k.token_hash == hash {
                    return Some(Resolved {
                        tenant_id: t.id.clone(),
                        key_id: k.id.clone(),
                        caps: k.caps.clone(),
                    });
                }
            }
        }
        None
    });
    let payload: Value = found.into();
    ok_result(payload)
}

#[export(name = "theater:inbox/registry.create-tenant")]
fn create_tenant(label: String) -> Value {
    let seed = match read_seed() {
        Ok(s) => s,
        Err(e) => return err_str(&e),
    };
    let (tenant_id, root_token) = RegistryState::with_mut(|s| {
        let tseq = s.next_seq;
        s.next_seq += 1;
        let tenant_id = format!("t{}", tseq);

        let kseq = s.next_seq;
        s.next_seq += 1;
        let key_id = format!("k{}", kseq);
        let token = to_hex(&hmac_sha256(&seed, format!("{}:{}", tenant_id, kseq).as_bytes()));
        let token_hash = sha256_hex(token.as_bytes());

        s.tenants.push(Tenant {
            id: tenant_id.clone(),
            label: label.clone(),
            keys: vec![Key {
                id: key_id,
                token_hash,
                caps: vec![CAP_USE.to_string(), CAP_ADMIN.to_string()],
                label: String::from("root"),
                created_seq: kseq,
                created_by: String::new(),
            }],
        });
        (tenant_id, token)
    });
    RegistryState::with(save_state);
    log(format!("[tenant-registry] created tenant {}", tenant_id));
    ok_result(TenantCreated { tenant_id, root_token }.into())
}

#[export(name = "theater:inbox/registry.mint-key")]
fn mint_key(tenant_id: String, caps: Vec<String>, label: String, created_by: String) -> Value {
    if let Err(e) = caps_valid(&caps) {
        return err_str(&e);
    }
    let seed = match read_seed() {
        Ok(s) => s,
        Err(e) => return err_str(&e),
    };
    let result = RegistryState::with_mut(|s| {
        let idx = match s.tenants.iter().position(|t| t.id == tenant_id) {
            Some(i) => i,
            None => return Err(format!("unknown tenant: {}", tenant_id)),
        };
        let kseq = s.next_seq;
        s.next_seq += 1;
        let key_id = format!("k{}", kseq);
        let token = to_hex(&hmac_sha256(&seed, format!("{}:{}", tenant_id, kseq).as_bytes()));
        let token_hash = sha256_hex(token.as_bytes());
        s.tenants[idx].keys.push(Key {
            id: key_id.clone(),
            token_hash,
            caps: caps.clone(),
            label: label.clone(),
            created_seq: kseq,
            created_by: created_by.clone(),
        });
        Ok((key_id, token))
    });
    match result {
        Ok((key_id, token)) => {
            RegistryState::with(save_state);
            ok_result(Minted { key_id, token }.into())
        }
        Err(e) => err_str(&e),
    }
}

/// Register an EXISTING token (its sha256) as a key on a tenant — instead of
/// generating one. The cutover primitive: grandfather the current shared bearer
/// token into the `fleet` tenant so it stays valid through the enforce flip.
/// RPC-only (operator setup), NOT HTTP-reachable. Rejects a token already
/// registered anywhere — a token must resolve to exactly one tenant, so the same
/// token can never be imported into two tenants (would be a cross-tenant hole).
#[export(name = "theater:inbox/registry.import-key")]
fn import_key(tenant_id: String, token: String, caps: Vec<String>, label: String) -> Value {
    if let Err(e) = caps_valid(&caps) {
        return err_str(&e);
    }
    if token.is_empty() {
        return err_str("import-key: empty token");
    }
    let token_hash = sha256_hex(token.as_bytes());
    let result = RegistryState::with_mut(|s| {
        // Global uniqueness: a token must map to exactly one tenant.
        if s.tenants.iter().any(|t| t.keys.iter().any(|k| k.token_hash == token_hash)) {
            return Err(String::from("token already registered"));
        }
        let idx = match s.tenants.iter().position(|t| t.id == tenant_id) {
            Some(i) => i,
            None => return Err(format!("unknown tenant: {}", tenant_id)),
        };
        let kseq = s.next_seq;
        s.next_seq += 1;
        let key_id = format!("k{}", kseq);
        s.tenants[idx].keys.push(Key {
            id: key_id.clone(),
            token_hash,
            caps: caps.clone(),
            label: label.clone(),
            created_seq: kseq,
            created_by: String::from("import"),
        });
        Ok(key_id)
    });
    match result {
        Ok(key_id) => {
            RegistryState::with(save_state);
            ok_result(Value::String(key_id))
        }
        Err(e) => err_str(&e),
    }
}

#[export(name = "theater:inbox/registry.revoke-key")]
fn revoke_key(key_id: String) -> Value {
    let removed = RegistryState::with_mut(|s| {
        let mut removed = false;
        for t in &mut s.tenants {
            let before = t.keys.len();
            t.keys.retain(|k| k.id != key_id);
            if t.keys.len() != before {
                removed = true;
            }
        }
        removed
    });
    if removed {
        RegistryState::with(save_state);
        log(format!("[tenant-registry] revoked key {}", key_id));
    }
    // Idempotent: revoking an unknown/already-gone key is still Ok.
    ok_unit()
}

#[export(name = "theater:inbox/registry.list-keys")]
fn list_keys(tenant_id: String) -> Value {
    let metas = RegistryState::with(|s| {
        s.tenants.iter().find(|t| t.id == tenant_id).map(|t| {
            t.keys
                .iter()
                .map(|k| KeyMeta {
                    key_id: k.id.clone(),
                    label: k.label.clone(),
                    caps: k.caps.clone(),
                    created_seq: k.created_seq,
                    created_by: k.created_by.clone(),
                })
                .collect::<Vec<_>>()
        })
    });
    match metas {
        Some(v) => ok_result(v.into()),
        None => err_str(&format!("unknown tenant: {}", tenant_id)),
    }
}
