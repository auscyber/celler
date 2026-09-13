# For distribution from this repository as well as CI, we use Crane to build
# Celler.

{ stdenv
, lib
, craneLib
, installShellFiles
, jq

# Build the server with the S3 storage backend, pulling in aws-sdk-s3.
, withS3 ? true
}:

let
  ignoredPaths = [
    ".ci"
    ".github"
    "book"
    "flake"
    "integration-tests"
    "nixos"
    "target"
  ];

  src = lib.cleanSourceWith {
    filter = name: type: !(type == "directory" && builtins.elem (baseNameOf name) ignoredPaths);
    src = lib.cleanSource ./.;
  };

  commonArgs = {
    pname = "celler";
    version = "0.1.0";

    inherit src;

    nativeBuildInputs = [
      # pkg-config
      installShellFiles
    ];

    buildInputs = [
      # Nothing yet.
    ];

    doCheck = false;

    CELLER_DISTRIBUTOR = "celler";
  };

  # Vendored once and shared by every derivation below, so the registry is not
  # realised twice.
  cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };

  depsArgs = commonArgs // { inherit cargoVendorDir; };

  # Feature selection for the whole-workspace build. Must match between a deps
  # layer and the package built on top of it, or the artifacts are refingerprinted.
  workspaceCargoArgsFor = s3: "--locked"
    + lib.optionalString (!s3) " --no-default-features --features attic/chunking,attic/io";

  workspaceCargoArgs = workspaceCargoArgsFor withS3;

  # Layer 1: only what the client binary needs.
  celler-client-deps = craneLib.buildDepsOnly (depsArgs // {
    pname = "celler-client";
    cargoExtraArgs = "--locked --package attic-client";
  });

  # Layer 2: the rest of the workspace, stacked on top of layer 1. Crane's
  # `buildDepsOnly` hardcodes `cargoArtifacts = null`, so drive
  # `mkCargoDerivation` directly to inherit the client's target directory
  # instead of rebuilding the shared dependencies.
  cellerDepsFor = s3: craneLib.mkCargoDerivation (depsArgs // {
    pnameSuffix = "-deps";
    src = craneLib.mkDummySrc commonArgs;

    cargoArtifacts = celler-client-deps;
    doInstallCargoArtifacts = true;

    buildPhaseCargoCommand = ''
      cargoWithProfile check ${workspaceCargoArgsFor s3}
      cargoWithProfile build ${workspaceCargoArgsFor s3}
    '';

    env.CRANE_BUILD_DEPS_ONLY = 1;
  });

  celler-deps = cellerDepsFor withS3;

  mkCeller = { pname, cargoArtifacts, cargoExtraArgs }: craneLib.buildPackage (depsArgs // {
    inherit pname cargoArtifacts cargoExtraArgs;

    postInstall = lib.optionalString (stdenv.hostPlatform == stdenv.buildPlatform) ''
      if [[ -f $out/bin/celler ]]; then
        installShellCompletion --cmd celler \
          --bash <($out/bin/celler gen-completions bash) \
          --zsh <($out/bin/celler gen-completions zsh) \
          --fish <($out/bin/celler gen-completions fish)
      fi
    '';

    meta = with lib; {
      description = "Multi-tenant Nix binary cache system";
      homepage = "https://github.com/blitz/celler";
      license = licenses.asl20;
      maintainers = with maintainers; [ blitz ];
      platforms = platforms.linux ++ platforms.darwin;
      mainProgram = "celler";
    };
  });

  celler-client = mkCeller {
    pname = "celler-client";
    cargoArtifacts = celler-client-deps;
    cargoExtraArgs = "--locked --package attic-client";
  };

  # Overridable per-derivation, so `celler.override { withS3 = false; }` works
  # through the overlay too, not just on the package set.
  celler = lib.makeOverridable (args: mkCeller {
    pname = "celler";
    cargoArtifacts = cellerDepsFor args.withS3;
    cargoExtraArgs = workspaceCargoArgsFor args.withS3;
  }) { inherit withS3; };

  # Celler interacts with Nix directly and its tests require trusted-user access
  # to nix-daemon to import NARs, which is not possible in the build sandbox.
  # In the CI pipeline, we build the test executable inside the sandbox, then
  # run it outside.
  celler-tests = craneLib.mkCargoDerivation (depsArgs // {
    pname = "celler-tests";

    cargoArtifacts = celler-deps;

    nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ jq ];

    doCheck = true;

    buildPhaseCargoCommand = "";
    checkPhaseCargoCommand = "cargoWithProfile test ${workspaceCargoArgs} --no-run --message-format=json >cargo-test.json";
    doInstallCargoArtifacts = false;

    installPhase = ''
      runHook preInstall

      mkdir -p $out/bin
      jq -r 'select(.reason == "compiler-artifact" and .target.test and .executable) | .executable' <cargo-test.json | \
        xargs -I _ cp _ $out/bin

      runHook postInstall
    '';
  });
in {
  inherit celler celler-client celler-tests celler-deps celler-client-deps;
}
