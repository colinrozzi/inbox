// Bakes the build id into the api-handler for GET /version (deploy-freshness).
// The build system (CI / the nix flake) sets $INBOX_BUILD_ID -- e.g. the git
// rev or release tag; absent it (a plain local build), the id is "unspecified".
// Read at compile time via env!("INBOX_BUILD_ID") in lib.rs. build.rs runs on
// the host (std available) even though the crate itself is no_std/wasm.
fn main() {
    let id = std::env::var("INBOX_BUILD_ID").unwrap_or_else(|_| "unspecified".to_string());
    println!("cargo:rustc-env=INBOX_BUILD_ID={id}");
    println!("cargo:rerun-if-env-changed=INBOX_BUILD_ID");
}
