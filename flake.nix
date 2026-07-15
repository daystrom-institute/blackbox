{
  description = "blackbox packages, apps, and contributor/dev-agent shells";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        blackbox = pkgs.rustPlatform.buildRustPackage {
          pname = "blackbox";
          version = "0.2.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "--workspace" ];
          cargoTestFlags = [ "--workspace" ];
        };

        mkProductApp = binName: flake-utils.lib.mkApp {
          drv = blackbox;
          exePath = "/bin/${binName}";
        };

        devHomeScript = "${self}/scripts/dev-agent-home.sh";

        mkWrapped = name: command: pkgs.writeShellApplication {
          inherit name;
          runtimeInputs = [ pkgs.bash ];
          text = ''
            exec "${devHomeScript}" exec ${command} "$@"
          '';
        };

        bbx-dev-home = pkgs.writeShellApplication {
          name = "bbx-dev-home";
          runtimeInputs = [ pkgs.bash ];
          text = ''
            exec "${devHomeScript}" "$@"
          '';
        };

        bbx-dev-blackboxd = mkWrapped "bbx-dev-blackboxd" "${blackbox}/bin/blackboxd";
        bbx-dev-blackopsd = mkWrapped "bbx-dev-blackopsd" "${blackbox}/bin/blackopsd";
        bbx-dev-fleetd = mkWrapped "bbx-dev-fleetd" "${blackbox}/bin/fleetd";
        bbx-dev-bro = mkWrapped "bbx-dev-bro" "${blackbox}/bin/bro";
        bbx-dev-claude = mkWrapped "bbx-dev-claude" ''"''${CLAUDE_BIN:-claude}"'';
        bbx-dev-codex = mkWrapped "bbx-dev-codex" ''"''${CODEX_BIN:-codex}"'';
        bbx-dev-gemini = mkWrapped "bbx-dev-gemini" ''"''${GEMINI_BIN:-gemini}"'';
        bbx-dev-copilot = mkWrapped "bbx-dev-copilot" ''"''${COPILOT_BIN:-gh}"'';
        bbx-dev-vibe = mkWrapped "bbx-dev-vibe" ''"''${VIBE_BIN:-vibe}"'';
      in {
        packages = {
          inherit blackbox;
          default = blackbox;
          dev-home = bbx-dev-home;
          dev-blackboxd = bbx-dev-blackboxd;
          dev-blackopsd = bbx-dev-blackopsd;
          dev-fleetd = bbx-dev-fleetd;
          dev-bro = bbx-dev-bro;
          dev-claude = bbx-dev-claude;
          dev-codex = bbx-dev-codex;
          dev-gemini = bbx-dev-gemini;
          dev-copilot = bbx-dev-copilot;
          dev-vibe = bbx-dev-vibe;
        };

        apps = {
          default = mkProductApp "blackboxd";
          blackboxd = mkProductApp "blackboxd";
          blackbox-corpusd = mkProductApp "blackbox-corpusd";
          blackopsd = mkProductApp "blackopsd";
          fleetd = mkProductApp "fleetd";
          bro-harness = mkProductApp "bro-harness";
          bro = mkProductApp "bro";
          dev-home = flake-utils.lib.mkApp { drv = bbx-dev-home; };
          dev-blackboxd = flake-utils.lib.mkApp { drv = bbx-dev-blackboxd; };
          dev-blackopsd = flake-utils.lib.mkApp { drv = bbx-dev-blackopsd; };
          dev-fleetd = flake-utils.lib.mkApp { drv = bbx-dev-fleetd; };
          dev-bro = flake-utils.lib.mkApp { drv = bbx-dev-bro; };
          dev-claude = flake-utils.lib.mkApp { drv = bbx-dev-claude; };
          dev-codex = flake-utils.lib.mkApp { drv = bbx-dev-codex; };
          dev-gemini = flake-utils.lib.mkApp { drv = bbx-dev-gemini; };
          dev-copilot = flake-utils.lib.mkApp { drv = bbx-dev-copilot; };
          dev-vibe = flake-utils.lib.mkApp { drv = bbx-dev-vibe; };
        };

        checks = {
          inherit blackbox;
          default = blackbox;
        };

        formatter = pkgs.nixpkgs-fmt;

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.bash
            pkgs.cargo
            pkgs.git
            pkgs.jq
            pkgs.nix
            pkgs.nixpkgs-fmt
            pkgs.pkg-config
            pkgs.rustc
          ];

          shellHook = ''
            export BBOX_DEV_REPO_ROOT="$(pwd)"
            echo "blackbox dev shell loaded"
            echo "consumer outputs: nix build .#blackbox | nix run .#blackboxd | nix run .#blackopsd | nix run .#fleetd | nix run .#bro"
            echo "isolated agent harness: nix run .#dev-home -- init"
          '';
        };

        devShells.dev-agent = pkgs.mkShell {
          packages = [
            pkgs.bash
            pkgs.cargo
            pkgs.git
            pkgs.jq
            pkgs.nix
            pkgs.nixpkgs-fmt
            pkgs.pkg-config
            pkgs.rustc
            bbx-dev-home
            bbx-dev-blackboxd
            bbx-dev-blackopsd
            bbx-dev-fleetd
            bbx-dev-bro
            bbx-dev-claude
            bbx-dev-codex
            bbx-dev-gemini
            bbx-dev-copilot
            bbx-dev-vibe
          ];

          shellHook = ''
            export BBOX_DEV_REPO_ROOT="$(pwd)"
            echo "blackbox dev-agent shell loaded"
            echo "run: bbx-dev-home init"
            echo "then: bbx-dev-blackboxd | bbx-dev-claude | bbx-dev-codex"
          '';
        };
      });
}
