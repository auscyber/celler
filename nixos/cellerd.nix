{
  lib,
  pkgs,
  config,
  ...
}:

let
  inherit (lib) types;

  cfg = config.services.cellerd;

  # unused when the entrypoint is flake
  flake = import ../flake-compat.nix;
  overlay = flake.defaultNix.overlays.default;

  format = pkgs.formats.toml { };

  checkedConfigFile =
    pkgs.runCommand "checked-celler-server.toml"
      {
        configFile = cfg.configFile;
      }
      ''
        cat $configFile

        export CELLER_SERVER_TOKEN_HS256_SECRET_BASE64="dGVzdCBzZWNyZXQ="
        export CELLER_SERVER_DATABASE_URL="sqlite://:memory:"
        ${cfg.package}/bin/cellerd --mode check-config -f $configFile
        cat <$configFile >$out
      '';

  celleradmShim = pkgs.writeShellScript "celleradm" ''
    if [ -n "$CELLERADM_PWD" ]; then
      cd "$CELLERADM_PWD"
      if [ "$?" != "0" ]; then
        >&2 echo "Warning: Failed to change directory to $CELLERADM_PWD"
      fi
    fi

    exec ${cfg.package}/bin/celleradm -f ${checkedConfigFile} "$@"
  '';

  # Empty rather than null, so the "environmentFile is not set" assertion is the
  # one that fires rather than an evaluation error.
  environmentFiles = lib.optionals (cfg.environmentFile != null) cfg.environmentFile;

  environmentFileFlags = lib.concatMapStringsSep " " (
    f: "--property=EnvironmentFile=${f}"
  ) environmentFiles;

  celleradmWrapper = pkgs.writeShellScriptBin "cellerd-celleradm" ''
    exec systemd-run \
      --quiet \
      --pipe \
      --pty \
      --wait \
      --collect \
      --service-type=exec \
      ${environmentFileFlags} \
      --property=DynamicUser=yes \
      --property=User=${cfg.user} \
      --property=Environment=CELLERADM_PWD=$(pwd) \
      --working-directory / \
      -- \
      ${celleradmShim} "$@"
  '';

  # The typed `tracing` options own the `[tracing]` section of the generated
  # config. The `otlp` table is only emitted when export is enabled, since the
  # server treats its mere presence as the switch.
  tracingSettings =
    lib.optionalAttrs (cfg.tracing.serviceName != null) {
      service-name = cfg.tracing.serviceName;
    }
    // lib.optionalAttrs cfg.tracing.otlp.enable {
      otlp = {
        inherit (cfg.tracing.otlp) protocol timeout;
        sample-ratio = cfg.tracing.otlp.sampleRatio;
      }
      // lib.optionalAttrs (cfg.tracing.otlp.endpoint != null) {
        inherit (cfg.tracing.otlp) endpoint;
      };
    };

  hasLocalPostgresDB =
    let
      url = cfg.settings.database.url or "";
      localStrings = [
        "localhost"
        "127.0.0.1"
        "/run/postgresql"
      ];
      hasLocalStrings = lib.any (lib.flip lib.hasInfix url) localStrings;
    in
    config.services.postgresql.enable && lib.hasPrefix "postgresql://" url && hasLocalStrings;
in
{
  imports = [
    (lib.mkRenamedOptionModule
      [ "services" "cellerd" "credentialsFile" ]
      [ "services" "cellerd" "environmentFile" ]
    )
  ];

  disabledModules = [ "services/networking/atticd.nix" ];

  options = {
    services.cellerd = {
      enable = lib.mkEnableOption "the cellerd, the Nix Binary Cache server";

      package = lib.mkPackageOption pkgs "celler" { };

      environmentFile = lib.mkOption {
        description = ''
          Path to an EnvironmentFile, or a list of them, containing required
          environment variables:

          - CELLER_SERVER_TOKEN_RS256_SECRET_BASE64: The base64-encoded RSA PEM PKCS1 of the
            RS256 JWT secret. Generate it with `openssl genrsa -traditional 4096 | base64 -w0`.

          This is also where any secret belonging to the OTLP exporter goes,
          since these files are the only part of the configuration that does not
          end up in the world-readable Nix store:

          - OTEL_EXPORTER_OTLP_HEADERS: headers sent to the collector, as
            `key=value,key2=value2`. Use it for authentication, e.g.
            `authorization=Basic ...` or `x-honeycomb-team=...`.
            OTEL_EXPORTER_OTLP_TRACES_HEADERS overrides it for span exports only.

          Taking a list lets each secret keep its own file, which is usually what
          a secret manager produces.
        '';
        type = types.nullOr (types.coercedTo types.path lib.singleton (types.listOf types.path));
        default = null;
      };

      user = lib.mkOption {
        description = ''
          The user under which celler runs.
        '';
        type = types.str;
        default = "cellerd";
      };

      group = lib.mkOption {
        description = ''
          The group under which celler runs.
        '';
        type = types.str;
        default = "cellerd";
      };

      settings = lib.mkOption {
        description = ''
          Structured configurations of cellerd.

          The `tracing` section is also reachable through the dedicated
          `services.cellerd.tracing` options, which is usually easier.
        '';
        type = format.type;
        default = { }; # setting defaults here does not compose well
      };

      logFilter = lib.mkOption {
        description = ''
          The `RUST_LOG` filter applied to what cellerd logs to the journal.

          Null leaves `RUST_LOG` unset.
        '';
        type = types.nullOr types.str;
        default = null;
        example = "info,attic_server=debug";
      };

      tracing = {
        serviceName = lib.mkOption {
          description = ''
            The `service.name` cellerd reports to the collector.
          '';
          type = types.nullOr types.str;
          default = null;
          example = "cellerd";
        };

        filter = lib.mkOption {
          description = ''
            The `CELLER_SERVER_OTEL_FILTER` filter deciding which spans and
            events are exported, independently of {option}`logFilter`.

            Null leaves the server default, which is `info`. Be careful raising
            this to `trace`: the exporter's own HTTP client is instrumented, so
            exporting its spans feeds back into itself.
          '';
          type = types.nullOr types.str;
          default = null;
          example = "info,attic_server=debug";
        };

        otlp = {
          enable = lib.mkEnableOption "exporting spans to an OpenTelemetry collector over OTLP";

          endpoint = lib.mkOption {
            description = ''
              The collector endpoint.

              Null falls back to the standard `OTEL_EXPORTER_OTLP_ENDPOINT` and
              `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` environment variables, which
              you can set through {option}`environmentFile`.
            '';
            type = types.nullOr types.str;
            default = null;
            example = "http://localhost:4317";
          };

          protocol = lib.mkOption {
            description = ''
              The wire protocol to reach the collector with.

              Collectors conventionally listen for `grpc` on port 4317 and for
              `http` on port 4318.
            '';
            type = types.enum [
              "grpc"
              "http"
            ];
            default = "grpc";
          };

          timeout = lib.mkOption {
            description = ''
              The timeout of a single export.
            '';
            type = types.str;
            default = "10s";
          };

          headers = lib.mkOption {
            description = ''
              Headers sent to the collector, set through
              `OTEL_EXPORTER_OTLP_HEADERS`.

              These end up in the world-readable Nix store, so keep them to
              non-secret routing headers such as a tenant ID. Anything that
              authenticates belongs in {option}`environmentFile`, which is read
              after this and therefore wins.

              Keys and values may not contain `,` or `=`.
            '';
            type = types.attrsOf types.str;
            default = { };
            example = {
              x-scope-orgid = "celler";
            };
          };

          sampleRatio = lib.mkOption {
            description = ''
              The fraction of traces to sample, from 0.0 to 1.0.
            '';
            type = types.numbers.between 0.0 1.0;
            default = 1.0;
            example = 0.1;
          };
        };
      };

      configFile = lib.mkOption {
        description = ''
          Path to an existing cellerd configuration file.

          By default, it's generated from `services.cellerd.settings`.
        '';
        type = types.path;
        default = format.generate "server.toml" cfg.settings;
        defaultText = "generated from `services.cellerd.settings`";
      };

      mode = lib.mkOption {
        description = ''
          Mode in which to run the server.

          'monolithic' runs all components, and is suitable for single-node deployments.

          'api-server' runs only the API server, and is suitable for clustering.

          'garbage-collector' only runs the garbage collector periodically.

          A simple NixOS-based Celler deployment will typically have one 'monolithic' and any number of 'api-server' nodes.

          There are several other supported modes that perform one-off operations, but these are the only ones that make sense to run via the NixOS module.
        '';
        type = lib.types.enum [
          "monolithic"
          "api-server"
          "garbage-collector"
        ];
        default = "monolithic";
      };

      # Internal flags
      useFlakeCompatOverlay = lib.mkOption {
        description = ''
          Whether to insert the overlay with flake-compat.
        '';
        type = types.bool;
        internal = true;
        default = true;
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.environmentFile != null;
        message = ''
          <option>services.cellerd.environmentFile</option> is not set.

          Run `openssl genrsa -traditional -out private_key.pem 4096 | base64 -w0` and create a file with the following contents:

          CELLER_SERVER_TOKEN_RS256_SECRET_BASE64="output from command"

          Then, set `services.cellerd.environmentFile` to the quoted absolute path of the file.
        '';
      }
      {
        assertion = !lib.any lib.isStorePath environmentFiles;
        message = ''
          <option>services.cellerd.environmentFile</option> points to a path in the Nix store. The Nix store is globally readable.

          You should use a quoted absolute path to prevent leaking secrets in the Nix store.
        '';
      }
    ];

    services.cellerd.settings = {
      database.url = lib.mkDefault "sqlite:///var/lib/cellerd/server.db?mode=rwc";

      # "storage" is internally tagged
      # if the user sets something the whole thing must be replaced
      storage = lib.mkDefault {
        type = "local";
        path = "/var/lib/cellerd/storage";
      };
    }
    // lib.optionalAttrs (tracingSettings != { }) {
      tracing = tracingSettings;
    };

    systemd.services.cellerd = {
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ] ++ lib.optionals hasLocalPostgresDB [ "postgresql.service" ];
      requires = lib.optionals hasLocalPostgresDB [ "postgresql.service" ];
      wants = [ "network-online.target" ];

      # Both of these are read from the environment rather than the config file.
      # Anything secret, such as `OTEL_EXPORTER_OTLP_HEADERS` carrying a vendor
      # API key, belongs in `environmentFile` instead: the unit environment ends
      # up in the world-readable Nix store.
      environment = lib.filterAttrs (_: v: v != null) {
        RUST_LOG = cfg.logFilter;
        CELLER_SERVER_OTEL_FILTER = cfg.tracing.filter;

        OTEL_EXPORTER_OTLP_HEADERS =
          if cfg.tracing.otlp.headers == { } then
            null
          else
            lib.concatStringsSep "," (lib.mapAttrsToList (k: v: "${k}=${v}") cfg.tracing.otlp.headers);
      };

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/cellerd -f ${checkedConfigFile} --mode ${cfg.mode}";
        EnvironmentFile = environmentFiles;
        StateDirectory = "cellerd"; # for usage with local storage and sqlite
        DynamicUser = true;
        User = cfg.user;
        Group = cfg.group;
        Restart = "on-failure";
        RestartSec = 10;

        CapabilityBoundingSet = [ "" ];
        DeviceAllow = "";
        DevicePolicy = "closed";
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        PrivateUsers = true;
        ProcSubset = "pid";
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectProc = "invisible";
        ProtectSystem = "strict";
        ReadWritePaths =
          let
            path = cfg.settings.storage.path;
            isDefaultStateDirectory = path == "/var/lib/cellerd" || lib.hasPrefix "/var/lib/cellerd/" path;
          in
          lib.optionals (cfg.settings.storage.type or "" == "local" && !isDefaultStateDirectory) [ path ];
        RemoveIPC = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@resources"
          "~@privileged"
        ];
        UMask = "0077";
      };
    };

    environment.systemPackages = [
      celleradmWrapper
    ];

    nixpkgs.overlays = lib.mkIf cfg.useFlakeCompatOverlay [
      overlay
    ];
  };
}
