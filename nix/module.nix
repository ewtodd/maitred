{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.maitred;
  format = pkgs.formats.json { };
  configFile = format.generate "maitred.json" cfg.settings;
  port = lib.toInt (lib.last (lib.splitString ":" cfg.settings.listen));
in
{
  options.services.maitred = {
    enable = lib.mkEnableOption "maitred, an OpenAI-compatible /v1 front for self-hosted LLM servers";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The maitred package (built with the host's nixpkgs by default).";
    };

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = format.type;
        options.listen = lib.mkOption {
          type = lib.types.str;
          default = "127.0.0.1:4000";
          example = "0.0.0.0:4000";
          description = "`host:port` to listen on.";
        };
      };
      default = { };
      description = ''
        The configuration file, as Nix. See the README for every field (`upstreams`, `models`, `modes`,
        `keys`, `mcp`, `passthrough`, `switch_hint`). Secrets do not go here: keys and upstream API keys
        are read from environment variables named in the settings, set through `environmentFiles`. MCP
        servers run as the service's user with HOME and a state directory at /var/lib/maitred.
      '';
    };

    environmentFiles = lib.mkOption {
      type = lib.types.listOf lib.types.path;
      default = [ ];
      example = [ "/run/secrets/maitred.env" ];
      description = "Files of `NAME=value` lines holding the secrets the settings name by variable.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the port in `settings.listen` in the firewall.";
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ port ];

    systemd.services.maitred = {
      description = "maitred: OpenAI-compatible /v1 front for LLM servers";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      restartTriggers = [ configFile ];
      # The MCP servers' home: some cache or store files there (pass e.g. a storage path under it).
      environment.HOME = "/var/lib/maitred";
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
        EnvironmentFile = cfg.environmentFiles;
        StateDirectory = "maitred";
        Restart = "always";
        RestartSec = 2;
        DynamicUser = true;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        LockPersonality = true;
        # MCP servers on Node (or any JIT) need writable-executable memory.
        MemoryDenyWriteExecute = (cfg.settings.mcp or { }) == { };
        SystemCallArchitectures = "native";
      };
    };
  };
}
