self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  settings = config.services.runboard;
  program =
    if settings.deploy.enable then
      "/var/lib/runboard-deploy/current/bin/runboard"
    else
      lib.getExe settings.package;
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
    deploy = {
      enable = lib.mkEnableOption "pull deployment of main revisions that pass GitHub CI";
      repository = lib.mkOption {
        type = lib.types.strMatching "[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+";
        default = "matterstate/runboard";
      };
      branch = lib.mkOption {
        type = lib.types.strMatching "[A-Za-z0-9_.-]+";
        default = "main";
      };
      healthUrl = lib.mkOption {
        type = lib.types.str;
        default = "http://${settings.listen}/healthz";
        description = "Health endpoint checked after activation and rollback.";
      };
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
      unitConfig.ConditionPathExists = program;
      serviceConfig = {
        ExecStart = "${program} ${lib.escapeShellArgs arguments}";
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
    users.users.runboard-deploy = lib.mkIf settings.deploy.enable {
      isSystemUser = true;
      group = "runboard-deploy";
      home = "/var/lib/runboard-deploy";
    };
    users.groups.runboard-deploy = lib.mkIf settings.deploy.enable { };
    systemd.services.runboard-deploy = lib.mkIf settings.deploy.enable {
      description = "Build and deploy runboard revisions that pass CI";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = [
        pkgs.curl
        pkgs.git
        pkgs.nix
        pkgs.systemd
      ];
      environment = {
        DEPLOY_REPOSITORY = settings.deploy.repository;
        DEPLOY_BRANCH = settings.deploy.branch;
        DEPLOY_HEALTH_URL = settings.deploy.healthUrl;
        GIT_TERMINAL_PROMPT = "0";
      };
      serviceConfig = {
        Type = "oneshot";
        User = "runboard-deploy";
        Group = "runboard-deploy";
        StateDirectory = "runboard-deploy";
        StateDirectoryMode = "0755";
        ExecCondition = "${settings.package}/bin/runboard-deploy check";
        ExecStart = "${settings.package}/bin/runboard-deploy build";
        ExecStartPost = "+${settings.package}/bin/runboard-deploy activate";
        TimeoutStartSec = "20min";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectSystem = "strict";
      };
    };
    systemd.timers.runboard-deploy = lib.mkIf settings.deploy.enable {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitInactiveSec = "2min";
        RandomizedDelaySec = 15;
        Unit = "runboard-deploy.service";
      };
    };
  };
}
