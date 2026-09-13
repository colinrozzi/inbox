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
      # Fleet 0.24 host, final tree rev cfcd7376 (PR #211) (self.pact + view-scoped control
      # + engine-axis #194 + packr 0.24 + supervisor handler dissolved into
      # runtime spawn/stop). Feeds packages.theater + the devShell (theaterBin =
      # the theater CLI, used by the spawn-verify path). The `nix build .#default`
      # wasm build does NOT use theaterBin — it pulls theater-guest as a Cargo git
      # dep (pinned to this same rev in the actor Cargo.tomls) + packr-guest 0.24;
      # nix's lazy eval never forces theaterBin for the default package. Manager
      # runs `nix flake update theater` on the dev box to sync flake.lock's
      # narHash to this rev (container agents can't nix-flake-update).
      url = "github:colinrozzi/theater/cfcd7376606758d388ab9d7042932d13e8da23a76";
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

        # Plain self-contained actor link flags (packr 0.11.0 recipe; supersedes
        # the 0.10.2 fixed-base/compose recipe entirely — no fixed base, no fused
        # allocator, no compose step. packr 0.11.0 links the allocator + growable
        # own-memory into the cdylib directly). MUST reach the real cargo
        # invocation. crane does NOT honor the repo .cargo/config.toml (kept
        # in-tree for devshell / plain-cargo builds), so pass them via
        # CARGO_ENCODED_RUSTFLAGS — highest cargo precedence, cannot be shadowed
        # by config. Flags are joined by 0x1f (ASCII unit separator), cargo's
        # encoded-rustflags delimiter, produced via fromJSON's escape. Keep this
        # list identical to .cargo/config.toml.
        #   --export-memory : export the cdylib's own growable linear memory (the
        #                     growable heap retires the 0.10.2 capped-heap decode
        #                     OOM class that blocked the mail-spine flip).
        #   --no-entry      : wasm reactor, no _start.
        rfSep = builtins.fromJSON "\"\\u001f\"";
        plainRustflags = builtins.concatStringsSep rfSep [
          "-C" "link-arg=--export-memory"
          "-C" "link-arg=--no-entry"
        ];

        commonArgs = {
          inherit src;
          pname = "inbox";
          version = "0.1.0";
          cargoExtraArgs = "--target wasm32-unknown-unknown";
          CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
          CARGO_ENCODED_RUSTFLAGS = plainRustflags;
          doCheck = false;
        };

        # cargoArtifacts=null: per the recipe's crane note, keep the wasm build
        # off a shared host cargoArtifacts (don't share host artifacts into the
        # wasm32-unknown-unknown build). One buildPackage pass.
        cargoArtifacts = null;

        theaterBin = theater.packages.${system}.default;

      in {
        # nix build — produces all seven plain self-contained actor modules in
        # $out as inbox_<actor>.wasm (the deployable 0.11.0 artifacts).
        #
        # packr 0.11.0 links each cdylib into a directly-loadable module: NO
        # `theater compose` step, NO binaryen/wasm-merge. crane builds the plain
        # members; the install phase asserts each is self-contained (every
        # `(import ...)` must be a host `theater:simple/*` — any env.memory,
        # pack:alloc, or __linear_memory import means the plain-build recipe was
        # not applied) and installs the bare $name.wasm. Only wasm-tools is
        # needed on PATH.
        packages.default = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          nativeBuildInputs = [ pkgs.wasm-tools ];
          installPhaseCommand = ''
            mkdir -p $out
            for name in \
              inbox_acceptor \
              inbox_api_handler \
              inbox_cli \
              inbox_mailbox \
              inbox_mailbox_router \
              inbox_smtp_acceptor \
              inbox_smtp_handler
            do
              wasm="target/wasm32-unknown-unknown/release/$name.wasm"
              wasm-tools validate "$wasm"
              bad=$(wasm-tools print "$wasm" | grep -E '^[[:space:]]*\(import ' | grep -v 'theater:simple/' || true)
              if [ -n "$bad" ]; then
                echo "ERROR: $name is NOT self-contained (non-host imports):"
                echo "$bad"
                exit 1
              fi
              cp "$wasm" "$out/$name.wasm"
              echo "$name.wasm: host imports only"
            done
          '';
        });

        # nix build .#theater — the pinned theater CLI (post-#204 c3937bdc); used
        # by the spawn-verify job's `theater setup`. Not used by packages.default.
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
          # packr 0.24 plain build: wasm-tools to build + verify (no compose), and
          # theaterBin = the post-#204 (c3937bdc) theater CLI for the spawn-verify
          # `theater setup` gate.
          packages = [ rustToolchain theaterBin pkgs.wasm-tools ];
          shellHook = ''
            echo "inbox dev environment (packr 0.24 plain build, theater cfcd7376)"
            echo "  cargo build --release --target wasm32-unknown-unknown   # directly-loadable <actor>.wasm, no compose"
            echo "  nix develop --command bash ops/spawn-verify.sh          # theater setup each composite"
          '';
        };
      });
}
