{
  description = "UPS Store application development environments";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      rust197 = pkgs.rust-bin.stable."1.97.1".default.override {
        extensions = [ "clippy" "rustfmt" ];
        targets = [ "wasm32-unknown-unknown" ];
      };
      rust193 = pkgs.rust-bin.stable."1.93.1".default.override {
        extensions = [ "clippy" "rust-analyzer" "rustfmt" ];
        targets = [ "wasm32-unknown-unknown" ];
      };

      pinnedNu = pkgs.stdenvNoCC.mkDerivation {
        pname = "nushell-bin";
        version = "0.112.2";
        src = pkgs.fetchurl {
          url = "https://github.com/nushell/nushell/releases/download/0.112.2/nu-0.112.2-x86_64-unknown-linux-gnu.tar.gz";
          hash = "sha256-QDjBcd0mGPJBOiqmFbjat+nQSFK+ggDxdV3z5CIyg5U=";
        };
        nativeBuildInputs = with pkgs; [ autoPatchelfHook gzip gnutar ];
        buildInputs = [ pkgs.stdenv.cc.cc.lib ];
        dontUnpack = true;
        installPhase = ''
          tar -xzf "$src"
          install -Dm755 nu-0.112.2-x86_64-unknown-linux-gnu/nu "$out/bin/nu"
        '';
      };

      pinnedPdfium = pkgs.pdfium-binaries.overrideAttrs (_: {
        version = "7881";
        src = pkgs.fetchzip {
          url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7881/pdfium-linux-x64.tgz";
          hash = "sha256-Z/2osUULxUn6hOqB2HM/MqcoT/HGS0/YCKrAHfx2xoE=";
          stripRoot = false;
        };
      });

      pinnedWasmBindgen = pkgs.buildWasmBindgenCli rec {
        src = pkgs.fetchCrate {
          pname = "wasm-bindgen-cli";
          version = "0.2.127";
          hash = "sha256-di+qBAdd7pENLiIB9CoZoab+W5xeDoByMREcCGTSzWo=";
        };
        cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
          inherit src;
          inherit (src) pname version;
          hash = "sha256-FTv2GZIAQs0ePdIZXIXil7JbZ6kIT05VG6vqC1qNFxQ=";
        };
      };

      pinnedPrintManagerWasmBindgen = pkgs.buildWasmBindgenCli rec {
        src = pkgs.fetchCrate {
          pname = "wasm-bindgen-cli";
          version = "0.2.108";
          hash = "sha256-UsuxILm1G6PkmVw0I/JF12CRltAfCJQFOaT4hFwvR8E=";
        };
        cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
          inherit src;
          inherit (src) pname version;
          hash = "sha256-iqQiWbsKlLBiJFeqIYiXo3cqxGLSjNM8SOWXGM9u43E=";
        };
      };

      pinnedNextest = pkgs.rustPlatform.buildRustPackage {
        pname = "cargo-nextest";
        version = "0.9.100";
        src = pkgs.fetchFromGitHub {
          owner = "nextest-rs";
          repo = "nextest";
          tag = "cargo-nextest-0.9.100";
          hash = "sha256-MbgX/n6TC5hz66gvRAc7A0xFWbF2Ec68gMxCgPFpeoQ=";
        };
        cargoHash = "sha256-jRBFjJB38JI9whFpImYlMx0znQj1+cdeu4Nc+nYc7OI=";
        cargoBuildFlags = [ "-p" "cargo-nextest" ];
        cargoTestFlags = [ "-p" "cargo-nextest" ];
        doCheck = false;
      };

      rust197ChannelManifest = pkgs.fetchurl {
        url = "https://static.rust-lang.org/dist/channel-rust-1.97.1.toml";
        hash = "sha256-A1abGIbOtcBSdrUMhDGrER3pRM1hQP4fp9gh3Y4PKc8=";
      };

      pdfRustToolchain = pkgs.runCommand "ups-store-pdf-rust-1.97.1" {
        nativeBuildInputs = [ pkgs.lndir ];
      } ''
        mkdir -p "$out"
        lndir -silent ${rust197} "$out"
        cp ${rust197ChannelManifest} "$out/lib/rustlib/multirust-channel-manifest.toml"
        printf '%s\n' \
          'config_version = "1"' \
          '[[components]]' \
          'pkg = "cargo"' \
          'target = "x86_64-unknown-linux-gnu"' \
          'is_extension = false' \
          '[[components]]' \
          'pkg = "clippy-preview"' \
          'target = "x86_64-unknown-linux-gnu"' \
          'is_extension = false' \
          '[[components]]' \
          'pkg = "rust-std"' \
          'target = "wasm32-unknown-unknown"' \
          'is_extension = false' \
          '[[components]]' \
          'pkg = "rust-std"' \
          'target = "x86_64-unknown-linux-gnu"' \
          'is_extension = false' \
          '[[components]]' \
          'pkg = "rustc"' \
          'target = "x86_64-unknown-linux-gnu"' \
          'is_extension = false' \
          '[[components]]' \
          'pkg = "rustfmt-preview"' \
          'target = "x86_64-unknown-linux-gnu"' \
          'is_extension = false' \
          > "$out/lib/rustlib/multirust-config.toml"
      '';

      pdfRustupHome = pkgs.runCommand "ups-store-pdf-rustup-home" { } ''
        mkdir -p "$out/toolchains" "$out/update-hashes"
        ln -s ${pdfRustToolchain} "$out/toolchains/1.97.1"
        ln -s ${pdfRustToolchain} "$out/toolchains/1.97.1-x86_64-unknown-linux-gnu"
        printf '%s\n' \
          'default_host_triple = "x86_64-unknown-linux-gnu"' \
          'default_toolchain = "1.97.1-x86_64-unknown-linux-gnu"' \
          'profile = "default"' \
          'version = "12"' \
          '[overrides]' > "$out/settings.toml"
      '';

      sharedPackages = with pkgs; [ agent-browser git jujutsu ] ++ [ pinnedNu ];
      mkShell = packages: pkgs.mkShell { packages = sharedPackages ++ packages; };
    in
    {
      devShells.${system} = {
        default = mkShell (with pkgs; [ just podman skopeo ]);

        pdf-app = (mkShell ([ pkgs.rustup rust197 ] ++ (with pkgs; [
          binaryen
          cargo-leptos
          clang
          chromium
          curl
          ffmpeg
          firefox
          gzip
          just
          pkg-config
          util-linux
        ]) ++ [ pinnedNextest pinnedPdfium pinnedWasmBindgen ])).overrideAttrs (_: {
          PDF_TOOLS_PDFIUM_PATH = "${pinnedPdfium}/lib/libpdfium.so";
          AGENT_BROWSER_EXECUTABLE_PATH = "${pkgs.chromium}/bin/chromium";
          # Bare CI containers have no system fonts; Chromium otherwise crashes
          # while rendering even static text, before browser smoke can run.
          FONTCONFIG_FILE = pkgs.makeFontsConf {
            fontDirectories = [ pkgs.dejavu_fonts ];
          };
          RUSTUP_HOME = pdfRustupHome;
          RUSTUP_TOOLCHAIN = "1.97.1-x86_64-unknown-linux-gnu";
        });

        print-manager = (mkShell ([ rust193 ] ++ (with pkgs; [
          cargo-leptos
          chromium
          just
          pkg-config
        ]) ++ [ pinnedNextest pinnedPrintManagerWasmBindgen ])).overrideAttrs (_: {
          AGENT_BROWSER_EXECUTABLE_PATH = "${pkgs.chromium}/bin/chromium";
        });

        auto-door = mkShell ([ rust193 pinnedNextest ] ++ (with pkgs; [ just ]));
      };

      checks.${system} = {
        tool-versions = pkgs.runCommand "ups-store-tool-versions" {
          nativeBuildInputs = [
            pinnedNu
            pinnedNextest
            pkgs.agent-browser
            pkgs.cargo-leptos
            pkgs.just
          ];
        } ''
          test "$(nu --no-config-file --version)" = 0.112.2
          test "$(cargo-nextest --version | head -n 1)" = "cargo-nextest 0.9.100"
          test "$(agent-browser --version)" = "agent-browser 0.27.0"
          test "$(cargo-leptos --version)" = "cargo-leptos 0.3.7"
          test "$(just --version)" = "just 1.58.0"
          touch "$out"
        '';

        rust-1_97 = pkgs.runCommand "ups-store-rust-1.97.1" {
          nativeBuildInputs = [ rust197 ];
        } ''
          test "$(rustc --version | cut -d ' ' -f 2)" = 1.97.1
          test -d "$(rustc --print sysroot)/lib/rustlib/wasm32-unknown-unknown"
          touch "$out"
        '';

        rust-1_93 = pkgs.runCommand "ups-store-rust-1.93.1" {
          nativeBuildInputs = [ rust193 ];
        } ''
          test "$(rustc --version | cut -d ' ' -f 2)" = 1.93.1
          test -d "$(rustc --print sysroot)/lib/rustlib/wasm32-unknown-unknown"
          touch "$out"
        '';

        pdf-rustup = pkgs.runCommand "ups-store-pdf-rustup-contract" {
          nativeBuildInputs = [ pkgs.rustup rust197 ];
          RUSTUP_HOME = pdfRustupHome;
          RUSTUP_TOOLCHAIN = "1.97.1-x86_64-unknown-linux-gnu";
        } ''
          test "$(cargo +1.97.1 --version | cut -d ' ' -f 2)" = 1.97.1
          rustup target list --installed | grep -Fx wasm32-unknown-unknown
          touch "$out"
        '';
      };
    };
}
