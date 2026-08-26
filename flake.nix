{
  description = "RustCraft — a WebGPU Minecraft-style voxel engine in Rust, browser build via Nix";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        f = fenix.packages.${system};
        # Host rustc + cargo (rustc already includes host rust-std) and
        # rust-std for the wasm target used by the browser build.
        rust = f.combine [
          f.stable.cargo
          f.stable.rustc
          f.targets."wasm32-unknown-unknown".stable."rust-std"
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rust
            # MUST stay in lockstep with the `wasm-bindgen` crate pin in Cargo.toml.
            pkgs.wasm-bindgen-cli
            # Headless browser + software Vulkan (lavapipe) for WebGPU verification.
            pkgs.chromium
            pkgs.mesa
            pkgs.vulkan-loader
            # Static file server for the built web app (+ HTTPS mode,
            # which WebGPU requires for anything other than localhost).
            pkgs.python3
            pkgs.openssl
            # Linker for host builds/tests, misc helpers for scripts.
            pkgs.gcc
            pkgs.jq
            pkgs.file
          ];
          shellHook = ''
            echo "=== RustCraft dev shell ==="
            echo "rustc:       $(rustc --version)"
            echo "wasm-bindgen: $(wasm-bindgen --version)"
            echo ""
            echo "Commands:"
            echo "  ./scripts/build.sh    build the wasm web app into web/dist"
            echo "  ./scripts/serve.sh    serve web/dist on http://localhost:8080"
            echo "  ./scripts/serve.sh --https  HTTPS (self-signed cert; needed off-localhost)"
            echo "  ./scripts/verify.sh   headless chromium smoke test + screenshot"
            echo "  cargo test            server/worldgen unit tests (host)"
          '';
        };
      });
}
