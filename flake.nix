{
  description = "maitred: an OpenAI-compatible /v1 front for self-hosted LLM servers that share GPUs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system: rec {
        maitred = nixpkgs.legacyPackages.${system}.callPackage ./nix/package.nix { };
        default = maitred;
      });

      # services.maitred; its package is built with the host's own nixpkgs.
      nixosModules.default = ./nix/module.nix;

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
