use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing::info;

/// Disk usage analyzer: what grew, what is freeable, what is cold.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Directory to scan
    #[arg(env = "SCAN_PATH", default_value = ".")]
    path: PathBuf,

    /// `tracing` filter directive (e.g. `info`, `camembert=debug`)
    ///
    /// Syntax: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives>
    #[arg(long = "log-filter", env = "LOG_FILTER", default_value = "info")]
    log_filter: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log_filter))
        .init();

    info!(path = %cli.path.display(), "starting");
    info!("scan engine not implemented yet — bootstrap only");
    ExitCode::SUCCESS
}
