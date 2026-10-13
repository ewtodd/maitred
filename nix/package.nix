{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "maitred";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        base = baseNameOf path;
      in
      !(builtins.elem base [
        "target"
        "result"
        "nix"
        "flake.nix"
        "flake.lock"
      ]);
  };

  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "OpenAI-compatible /v1 front for self-hosted LLM servers that share GPUs";
    homepage = "https://github.com/ewtodd/maitred";
    license = lib.licenses.mit;
    mainProgram = "maitred";
  };
}
