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
      url = "github:colinrozzi/theater/release-20260510";
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

        commonArgs = {
          inherit src;
          pname = "inbox";
          version = "0.1.0";
          cargoExtraArgs = "--target wasm32-unknown-unknown";
          CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
          doCheck = false;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        theaterBin = theater.packages.${system}.default;

      in {
        # nix build — produces all six .wasm artifacts in $out
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
          packages = [ rustToolchain theaterBin ];
          shellHook = ''
            echo "inbox dev environment"
            echo "  cargo build --release --target wasm32-unknown-unknown"
            echo "  theater start acceptor/manifest.toml"
            echo "  curl http://localhost:8080/v1/inbox"
          '';
        };
      });
}
