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
      # Fleet rev = theater packr 0.10.4 host (PR #147): the ONE rev every actor's
      # theater input pins AND the rev the prod binary is cut from (the atomic-flip
      # contract). 0.10.4 = the decoder peak-mem fix; the host bump is alignment
      # (host runs uncapped), the load-bearing fix is the guest packr-guest 0.10.4
      # bump (the actor Cargo.tomls). theaterBin from here does `theater compose`.
      # Manager bumps flake.lock's narHash on the dev box (container agents can't
      # nix-flake-update).
      url = "github:colinrozzi/theater/6f04a4dc72e3efce067997d2ec0d763f9ea0362b";
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
        # nix build — produces all seven self-contained COMPOSITES in $out as
        # inbox_<actor>.composite.wasm (the deployable 0.10.2 artifacts).
        #
        # crane builds the bare fixed-base members; `theater compose` then fuses
        # each member + packr's bundled allocator into an own-memory composite
        # (single-package, base 0x50000) and verifies it (residual imports must
        # be host theater:simple/* only — no env.memory / pack:alloc /
        # __linear_memory). theater's 0.10.x loader (assert_self_contained)
        # rejects a bare member, so ONLY the composites are installed. `theater
        # compose` verifies by default and fails the build on a non-self-contained
        # artifact, so a bad member never installs. Needs theaterBin (0.10.2 rev,
        # pinned above) + wasm-merge (binaryen) + wasm-tools on PATH.
        packages.default = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          nativeBuildInputs = [ theaterBin pkgs.binaryen pkgs.wasm-tools ];
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
              theater compose \
                "target/wasm32-unknown-unknown/release/$name.wasm" \
                -o "$out/$name.composite.wasm"
            done
          '';
        });

        # Debug-only variant for the 0.10.2 acceptor-hang repro (theater-dev's
        # test C): identical to packages.default but --initial-memory bumped
        # 8MiB -> 64MiB. Discriminates the init spin: boots with headroom =>
        # memory-grow-thrash (interim --initial-memory bump could unblock the
        # flip); still thrashes at 64MiB => algorithmic O(n^2) decode/alloc that
        # only pack-dev's fix resolves. NOT a deploy artifact — packages.default
        # (the deployable) is unchanged.
        packages.composites-64m = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          CARGO_ENCODED_RUSTFLAGS = builtins.concatStringsSep rfSep [
            "-C" "link-arg=--import-memory"
            "-C" "link-arg=--initial-memory=67108864"
            "-C" "link-arg=--stack-first"
            "-C" "link-arg=-zstack-size=262144"
            "-C" "link-arg=--global-base=327680"
            "-C" "link-arg=--no-entry"
            "-C" "link-arg=--no-merge-data-segments"
          ];
          nativeBuildInputs = [ theaterBin pkgs.binaryen pkgs.wasm-tools ];
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
              theater compose \
                "target/wasm32-unknown-unknown/release/$name.wasm" \
                -o "$out/$name.composite.wasm"
            done
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
