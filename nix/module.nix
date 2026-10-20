self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  settings = config.services.runboard;
  arguments = [
    "--state-dir"
    "/var/lib/runboard"
    "--listen"
    settings.listen
  ]
  ++ lib.concatMap (root: [
    "--root"
    root
  ]) settings.roots
  ++ (
    if settings.natsUrl == null then
      [
        "--nats-port"
        (toString settings.natsPort)
      ]
    else
      [
        "--nats-url"
        settings.natsUrl
      ]
  )
  ++ lib.optionals (settings.pueueConfig != null) [
    "--pueue"
    settings.pueueConfig
  ]
  ++ lib.optionals settings.artifacts [ "--artifacts" ];
in
{
  options.services.runboard = {
    enable = lib.mkEnableOption "runboard with a managed NATS sidecar";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    };
    roots = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Named project roots, for example training=/srv/training.";
    };
    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:5000";
    };
    natsPort = lib.mkOption {
      type = lib.types.port;
      default = 4223;
    };
    natsUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "External broker endpoint. Null starts the bundled private broker.";
    };
    pueueConfig = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
    };
    artifacts = lib.mkEnableOption "artifact previews and downloads";
    user = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Existing user for private project files and Pueue. Null uses a dynamic user.";
    };
  };
  config = lib.mkIf settings.enable {
    assertions = [
      {
        assertion = settings.roots != [ ];
        message = "services.runboard.roots must contain at least one project.";
      }
    ];
    systemd.services.runboard = {
      description = "Run metrics, queue state and telemetry";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      serviceConfig = {
        ExecStart = "${lib.getExe settings.package} ${lib.escapeShellArgs arguments}";
        DynamicUser = settings.user == null;
        StateDirectory = "runboard";
        StateDirectoryMode = "0700";
        KillMode = "control-group";
        TimeoutStopSec = 15;
        Restart = "on-failure";
        RestartSec = 3;
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectHome = "read-only";
        ProtectSystem = "strict";
      }
      // lib.optionalAttrs (settings.user != null) { User = settings.user; };
    };
  };
}
