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
      # Runtime host reference (packages.theater + devShell only). As of the
      # packr 0.11.0 plain-build migration the composite build no longer uses
      # theaterBin at all (no `theater compose` step), so this input does NOT
      # gate `nix build .#default` — nix's lazy eval never forces theaterBin for
      # the default package. Still pinned at the 0.10.6 host (516c4b7e, #148) so
      # packages.theater/devShell resolve; the bump to the 0.11.0 host
      # (theater PR #149, 73a4540b) is a FOLLOW-UP gated on theater-dev cutting +
      # verifying the 0.11.0 host binary (a 0.10.6 host cannot load 0.11.0
      # plain-build actors, so local `theater spawn` in the devShell is stale
      # until that bump). Manager bumps flake.lock's narHash on the dev box
      # (container agents can't nix-flake-update).
      url = "github:colinrozzi/theater/516c4b7eec38998b18158e1ea9720cbf0716685a";
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

        # nix build .#theater — exposes the pinned theater binary. NOTE: still the
        # 0.10.6 host (see the `theater` input comment); the 0.11.0 host bump is a
        # follow-up. Not used by packages.default.
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
          # packr 0.11.0 plain build: only wasm-tools is needed to build + verify
          # (no binaryen/wasm-merge — the compose step is gone). theaterBin is
          # kept for local `theater spawn`, but note it is still the 0.10.6 host
          # until the 0.11.0 host bump lands, so it cannot spawn a 0.11.0 actor.
          packages = [ rustToolchain theaterBin pkgs.wasm-tools ];
          shellHook = ''
            echo "inbox dev environment (packr 0.11.0 plain build)"
            echo "  cargo build --release --target wasm32-unknown-unknown   # directly-loadable <actor>.wasm, no compose"
            echo "  wasm-tools print <actor>.wasm | grep '(import'          # verify: host theater:simple/* only"
          '';
        };
      });
}
