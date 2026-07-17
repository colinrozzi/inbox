{
  description = "Inbox: agent-first email service built on Theater";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";

    theater = {
      url = "github:colinrozzi/theater";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
      inputs.crane.follows = "crane";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, theater }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            (pkgs.lib.hasSuffix ".rs" path) ||
            (pkgs.lib.hasSuffix ".toml" path) ||
            (pkgs.lib.hasSuffix ".lock" path) ||
            (type == "directory");
        };

        # Fixed-base self-contained member link flags (packr 0.10.2 recipe;
        # supersedes the 0.8.1 PIC side-module flags — PIC is gone). MUST reach
        # the real cargo invocation. crane does NOT honor the repo
        # .cargo/config.toml (kept in-tree for devshell / plain-cargo builds),
        # so pass them via CARGO_ENCODED_RUSTFLAGS — highest cargo precedence,
        # cannot be shadowed by config. Flags are joined by 0x1f (ASCII unit
        # separator), cargo's encoded-rustflags delimiter, produced here via
        # fromJSON's  escape.
        # Keep this list identical to .cargo/config.toml. --global-base=327680
        # (0x50000) is the single-package base; all seven inbox actors are
        # single-package (no [[link]] edges) so it applies to every member.
        rfSep = builtins.fromJSON "\"\\u001f\"";
        fixedBaseRustflags = builtins.concatStringsSep rfSep [
          "-C" "link-arg=--import-memory"
          "-C" "link-arg=--initial-memory=8388608"
          "-C" "link-arg=--stack-first"
          "-C" "link-arg=-zstack-size=262144"
          "-C" "link-arg=--global-base=327680"
          "-C" "link-arg=--no-entry"
          "-C" "link-arg=--no-merge-data-segments"
        ];

        commonArgs = {
          inherit src;
          pname = "inbox";
          version = "0.1.0";
          cargoExtraArgs = "--target wasm32-unknown-unknown";
          CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
          CARGO_ENCODED_RUSTFLAGS = fixedBaseRustflags;
          doCheck = false;
        };

        # cargoArtifacts=null: per the recipe's crane note, keep the wasm member
        # build off a shared host cargoArtifacts (don't share host artifacts into
        # the wasm32-unknown-unknown build). One buildPackage pass.
        cargoArtifacts = null;

        theaterBin = theater.packages.${system}.default;

      in {
        # nix build — produces all seven cargo-built .wasm MEMBERS in $out.
        #
        # ⚠️ 0.10.2 cutover, STILL PENDING: these are bare fixed-base members,
        # NOT the deployable self-contained composites. theater's 0.10.x loader
        # (assert_self_contained) REJECTS a bare member. The composite = member +
        # packr bundled allocator, fused via packr::link (`wasm-merge`), verified
        # with wasm-tools (residual imports must be host theater:simple/* only).
        # This flake does not yet run that compose step because it needs (a) the
        # `theater` input bumped to the 0.10.2 rev so theaterBin ships the
        # compose tooling, and (b) a crane-friendly way to invoke it on a
        # prebuilt member (the shipped `theater build` re-runs cargo; there is no
        # standalone `theater compose <member>` CLI yet). Tracked with theater-dev.
        # Until then, `nix build` validates ONLY that the members compile under
        # 0.10.2 + the fixed-base recipe — it does not emit loadable artifacts.
        packages.default = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          installPhaseCommand = ''
            mkdir -p $out
            cp target/wasm32-unknown-unknown/release/inbox_acceptor.wasm $out/
            cp target/wasm32-unknown-unknown/release/inbox_api_handler.wasm $out/
            cp target/wasm32-unknown-unknown/release/inbox_cli.wasm $out/
            cp target/wasm32-unknown-unknown/release/inbox_mailbox.wasm $out/
            cp target/wasm32-unknown-unknown/release/inbox_mailbox_router.wasm $out/
            cp target/wasm32-unknown-unknown/release/inbox_smtp_acceptor.wasm $out/
            cp target/wasm32-unknown-unknown/release/inbox_smtp_handler.wasm $out/
          '';
        });

        # nix build .#theater — exposes the pinned theater binary used at runtime
        packages.theater = theaterBin;

        packages.clippy = craneLib.cargoClippy (commonArgs // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--target wasm32-unknown-unknown -- -D warnings";
        });

        packages.fmt = craneLib.cargoFmt {
          inherit src;
          pname = "inbox";
          version = "0.1.0";
        };

        devShells.default = craneLib.devShell {
          # binaryen (wasm-merge) + wasm-tools are required by `theater build`
          # to compose + verify the self-contained composite (packr 0.10.2).
          packages = [ rustToolchain theaterBin pkgs.binaryen pkgs.wasm-tools ];
          shellHook = ''
            echo "inbox dev environment"
            echo "  cargo build --release --target wasm32-unknown-unknown   # bare member"
            echo "  theater build --release <actor-dir>                     # self-contained composite (needs theater 0.10.2)"
            echo "  theater spawn acceptor/manifest.toml"
          '';
        };
      });
}
