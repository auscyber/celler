use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use tokio::join;
use tokio::task::spawn;
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use attic_server::config;
use attic_server::telemetry::{self, OtelLayer};

/// Nix binary cache server.
#[derive(Debug, Parser)]
#[clap(version, author = "Zhaofeng Li <hello@zhaofeng.li>")]
#[clap(propagate_version = true)]
struct Opts {
    /// Path to the config file.
    #[clap(short = 'f', long)]
    config: Option<PathBuf>,

    /// Socket address to listen on.
    ///
    /// This overrides `listen` in the config.
    #[clap(short = 'l', long)]
    listen: Option<SocketAddr>,

    /// Mode to run.
    #[clap(long, default_value = "monolithic")]
    mode: ServerMode,

    /// Whether to enable tokio-console.
    ///
    /// The console server will listen on its default port.
    #[clap(long)]
    tokio_console: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ServerMode {
    /// Run all components.
    Monolithic,

    /// Run the API server.
    ApiServer,

    /// Run the garbage collector periodically.
    GarbageCollector,

    /// Run the database migrations then exit.
    DbMigrations,

    /// Run garbage collection then exit.
    GarbageCollectorOnce,

    /// Check the configuration then exit.
    CheckConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    dump_version();

    // The config decides whether spans are exported, and a per-layer filter can
    // only be registered while the subscriber is being built, so the config has
    // to be read first.
    let config =
        config::load_config(opts.config.as_deref(), opts.mode == ServerMode::Monolithic).await?;

    // `check-config` runs inside the Nix build sandbox, where opening an
    // exporter has nothing to talk to.
    let tracing_config = if opts.mode == ServerMode::CheckConfig {
        Default::default()
    } else {
        config.tracing.clone()
    };

    // Held until the end of `main` so the batch processor gets flushed.
    let (otel_layer, _telemetry) = telemetry::init(&tracing_config)?;

    init_logging(opts.tokio_console, otel_layer);

    match opts.mode {
        ServerMode::Monolithic => {
            attic_server::run_migrations(config.clone()).await?;

            let (api_server, _) = join!(
                attic_server::run_api_server(opts.listen, config.clone()),
                attic_server::gc::run_garbage_collection(config.clone()),
            );

            api_server?;
        }
        ServerMode::ApiServer => {
            attic_server::run_api_server(opts.listen, config).await?;
        }
        ServerMode::GarbageCollector => {
            attic_server::gc::run_garbage_collection(config.clone()).await;
        }
        ServerMode::DbMigrations => {
            attic_server::run_migrations(config).await?;
        }
        ServerMode::GarbageCollectorOnce => {
            attic_server::gc::run_garbage_collection_once(config).await?;
        }
        ServerMode::CheckConfig => {
            // config is valid, let's just exit :)
        }
    }

    Ok(())
}

fn init_logging(tokio_console: bool, otel_layer: OtelLayer) {
    let env_filter = EnvFilter::from_default_env();
    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(env_filter);

    let error_layer = ErrorLayer::default();

    let console_layer = if tokio_console {
        let (layer, server) = console_subscriber::ConsoleLayer::new();
        spawn(server.serve());
        Some(layer)
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(otel_layer)
        .with(fmt_layer)
        .with(error_layer)
        .with(console_layer)
        .init();

    if tokio_console {
        eprintln!("Note: tokio-console is enabled");
    }
}

fn dump_version() {
    #[cfg(debug_assertions)]
    eprintln!("Celler {} (debug)", env!("CARGO_PKG_VERSION"));

    #[cfg(not(debug_assertions))]
    eprintln!("Celler {} (release)", env!("CARGO_PKG_VERSION"));
}
