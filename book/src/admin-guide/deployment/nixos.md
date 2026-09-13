# Deploying to NixOS

Celler provides [a NixOS module](https://github.com/blitz/celler/blob/main/nixos/cellerd.nix) that allows you to deploy the Celler Server on a NixOS machine.

## Prerequisites

1. A machine running NixOS
1. _(Optional)_ A dedicated bucket on S3 or a S3-compatible storage service
    - You can either [set up Garage](https://search.nixos.org/options?query=services.garage) or use a hosted service like [Backblaze B2](https://www.backblaze.com/b2/docs) and [Cloudflare R2](https://developers.cloudflare.com/r2).
1. _(Optional)_ A PostgreSQL database

## Generating the Credentials File

The RS256 JWT secret can be generated with the `openssl` utility:

```bash
nix run nixpkgs#openssl -- genrsa -traditional 4096 | base64 -w0
```

Create a file on the server containing the following contents:

```
CELLER_SERVER_TOKEN_RS256_SECRET_BASE64="output from above"
```

Ensure the file is only accessible by root.

## Importing the Module

You can import the module in one of two ways:

- Ad-hoc: Import the `nixos/cellerd.nix` from [the repository](https://github.com/blitz/celler).
- Flakes: Add `github:blitz/celler` as an input, then import `celler.nixosModules.cellerd`.

## Configuration

> Note: These options are subject to change.

```nix
{
  services.cellerd = {
    enable = true;

    # Replace with absolute path to your environment file
    environmentFile = "/etc/cellerd.env";

    settings = {
      listen = "[::]:8080";

      jwt = { };

      # Data chunking
      #
      # Warning: If you change any of the values here, it will be
      # difficult to reuse existing chunks for newly-uploaded NARs
      # since the cutpoints will be different. As a result, the
      # deduplication ratio will suffer for a while after the change.
      chunking = {
        # The minimum NAR size to trigger chunking
        #
        # If 0, chunking is disabled entirely for newly-uploaded NARs.
        # If 1, all NARs are chunked.
        nar-size-threshold = 64 * 1024; # 64 KiB

        # The preferred minimum size of a chunk, in bytes
        min-size = 16 * 1024; # 16 KiB

        # The preferred average size of a chunk, in bytes
        avg-size = 64 * 1024; # 64 KiB

        # The preferred maximum size of a chunk, in bytes
        max-size = 256 * 1024; # 256 KiB
      };
    };
  };
}
```

After the new configuration is deployed, the Celler Server will be accessible on port 8080.
It's highly recommended to place it behind a reverse proxy like [NGINX](https://nixos.wiki/wiki/Nginx) to provide HTTPS.

## Tracing

The server exports spans over OTLP, and returns correlation headers on every response:

- `X-Celler-Op-Id`: the trace ID of the request, rendered as a UUID. It also
  appears in error bodies, so an ID a user quotes can be pasted straight into a
  trace search.
- `X-Request-Id`: echoed back when the client or a proxy sent one, generated
  otherwise.
- `traceparent`: the W3C trace context of the response. An inbound `traceparent`
  is never adopted — every request starts a fresh root trace.

```nix
{
  services.cellerd = {
    # What the server logs to the journal
    logFilter = "info,attic_server=debug";

    tracing = {
      serviceName = "cellerd-prod";

      # What gets exported, independently of logFilter
      filter = "info";

      otlp = {
        enable = true;
        endpoint = "http://otel-collector:4317";
        protocol = "grpc"; # or "http", conventionally on port 4318
        sampleRatio = 0.1;

        # Non-secret headers only: these land in the Nix store
        headers.x-scope-orgid = "celler";
      };
    };
  };
}
```

### Authenticating to the collector

Anything that authenticates must not go in `settings` or `tracing.otlp.headers`,
because the generated configuration and the unit environment both end up in the
world-readable Nix store. Put it in an `environmentFile` instead, which systemd
reads after the unit environment and which therefore wins:

```
OTEL_EXPORTER_OTLP_HEADERS="authorization=Basic aW5zdGFuY2U6dG9rZW4="
```

`environmentFile` also accepts a list, so an OTLP token can live in its own file
alongside the JWT secret:

```nix
{
  services.cellerd.environmentFile = [
    "/run/secrets/cellerd-jwt.env"
    "/run/secrets/cellerd-otlp.env"
  ];
}
```

The exporter honours the standard OTLP environment variables, so
`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` and friends
work from the same files. Leaving `tracing.otlp.endpoint` null defers to them.

## Operations

The NixOS module installs the `cellerd-celleradm` wrapper which runs the `celleradm` command as the `cellerd` user.
Use this command to [generate new tokens](../../reference/celleradm-cli.md#celleradm-make-token) to be distributed to users.
