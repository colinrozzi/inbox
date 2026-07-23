# bindings-add — offline router-bindings recovery tool

A one-shot Theater ops actor that adds addresses to the inbox **router-bindings**
blob **in the store directly**, without mutating a live router. Built for
recovering the 3 addresses (`claude@`, `manager@`, `theater-dev@`) that re-flip
attempts dropped from the blob.

It reuses the router's exact `Binding` type + packr-guest encode/decode + store
API, so what it writes is byte-compatible with what the router reads.

## Why an actor (not a raw-filesystem edit)

Encoding a `Vec<Binding>` by hand risks a wrong wire format that silently
corrupts the blob. This actor calls the same `packr_guest` encode/decode + the
`theater:simple/store` API the router uses, so wire compatibility is guaranteed.
It runs against the live store during flip **staging** (router stopped), so there
is no live-router mutation and no wedge surface.

## Safety properties

- **Dry-run by default.** A bare `initial_state = "addr1,addr2,..."` only reports
  what it *would* do. It writes only with a `WRITE:` prefix.
- **Round-trip self-check.** Before trusting its own encoder it decodes the
  current blob and re-encodes it unchanged; if the bytes are not identical it
  **aborts** (its encoding does not reproduce the stored format). This also
  proves the 0.11.0 stack can read the baseline-written blob.
- **Idempotent.** An address already present is skipped, never duplicated.
- **Revert.** `store-at-label` writes a NEW content-addressed blob and repoints
  the label; the OLD blob is untouched. The run logs `OLD_REF -> NEW_REF`.
  Revert = repoint the `router-bindings` label back to `OLD_REF`. Record OLD_REF
  before applying (expected `10002012966b0038a0cf9f23c5a34d505c576ce9`).

## Build (dev box — plain 0.11.0 recipe, no compose)

```sh
cd tools/bindings-add
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/bindings_add.wasm  (directly loadable)
# verify host-only imports:
wasm-tools print target/wasm32-unknown-unknown/release/bindings_add.wasm \
  | grep '(import' | grep -v 'theater:simple/'    # must print NOTHING
```

## Run (VPS, during 0.11.0 flip staging, router STOPPED)

1. Edit `manifest.toml`: set `package` to the built wasm and confirm the store
   handler `base_path`/`store_id` match prod (`/mnt/main-volume/inbox/store`,
   `inbox`).
2. **Dry run first** (default initial_state, no `WRITE:`):
   ```sh
   theater spawn manifest.toml    # (with the 0.11.0 theater binary)
   ```
   Read the log: confirm `ROUND-TRIP OK`, the existing-binding list, and the
   `ADD:`/`SKIP:` lines. If it logs `ROUND-TRIP MISMATCH`, STOP and report — do
   not write.
3. **Apply**: change `initial_state` to prefix `WRITE:` and spawn again. The log
   prints `WROTE router-bindings: OLD_REF=... -> NEW_REF=...`. Record NEW_REF.
4. Start the 0.11.0 router; it loads all addresses (incl. the 3) from the blob
   at startup. Confirm with `GET /v1/mailboxes` (all present) + a test read of
   each recovered address.

## initial_state grammar

```
"[WRITE:]addr1,addr2,..."
```
- no prefix → **dry run** (default; writes nothing)
- `WRITE:` prefix → apply (repoint the label)
- whitespace around each address is trimmed; empty entries ignored
