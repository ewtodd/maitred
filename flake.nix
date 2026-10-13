{
  description = "llm-gateway: an OpenAI-compatible /v1 front for the house's LLM servers (virtual keys, routing, model metadata, effort clamping, mode-aware errors)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      # Callers (the NixOS host) pass their own pkgs so there is one nixpkgs.
      lib.mkLlmGateway = { pkgs }: pkgs.callPackage ./nix/package.nix { };

      packages = forAllSystems (system: rec {
        llm-gateway = self.lib.mkLlmGateway { pkgs = nixpkgs.legacyPackages.${system}; };
        default = llm-gateway;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
