{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "llm-gateway";
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
    description = "OpenAI-compatible /v1 gateway: virtual keys, routing, model metadata, effort clamping, mode-aware errors";
    license = lib.licenses.mit;
    mainProgram = "llm-gateway";
  };
}
