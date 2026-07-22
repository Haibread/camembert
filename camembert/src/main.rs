use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use tracing::{error, info};

use camembert_core::scan::{ScanOptions, Scanner};
use camembert_core::size::HumanSize;

/// Disk usage analyzer: what grew, what is freeable, what is cold.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Directory to scan (env: SCAN_PATH)
    #[arg(env = "SCAN_PATH", default_value = ".")]
    path: PathBuf,

    /// `tracing` filter directive (e.g. `info`, `camembert=debug`) (env: LOG_FILTER)
    ///
    /// Syntax: <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives>
    #[arg(long = "log-filter", env = "LOG_FILTER", default_value = "info")]
    log_filter: String,

    /// Scan worker threads; 0 = auto (2x CPU cores, capped at 8) (env: THREADS)
    #[arg(long, env = "THREADS", default_value_t = 0)]
    threads: usize,

    /// Cross filesystem boundaries instead of stopping at mount points (env: CROSS_FILESYSTEMS)
    #[arg(long, env = "CROSS_FILESYSTEMS")]
    cross_filesystems: bool,

    /// Number of directories in the "top directories by real size" list (env: TOP)
    #[arg(long, env = "TOP", default_value_t = 20)]
    top: usize,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log_filter))
        .with_writer(std::io::stderr)
        .init();

    let scanner = Scanner::new(ScanOptions {
        threads: cli.threads,
        cross_filesystems: cli.cross_filesystems,
    });

    // Progress line on stderr (via tracing) roughly every second while the
    // scan blocks this thread.
    let progress = scanner.progress();
    let done = Arc::new(AtomicBool::new(false));
    let poller = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                if done.load(Ordering::Acquire) {
                    break;
                }
                info!(
                    entries = progress.entries(),
                    dirs = progress.dirs(),
                    errors = progress.errors(),
                    disk = %HumanSize(progress.disk_bytes()),
                    "scanning"
                );
            }
        })
    };

    let outcome = scanner.scan(&cli.path);
    done.store(true, Ordering::Release);
    let _ = poller.join();

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            error!(%err, "scan failed");
            return ExitCode::FAILURE;
        }
    };

    // Product output on stdout (diagnostics stay on stderr via tracing).
    println!(
        "Scanned {} in {:.2}s",
        cli.path.display(),
        outcome.elapsed.as_secs_f64()
    );
    println!(
        "  total: {} real, {} apparent",
        HumanSize(outcome.totals.real),
        HumanSize(outcome.totals.apparent)
    );
    print!(
        "  entries: {} ({} dirs)  errors: {}  excluded (other fs): {}",
        outcome.entries, outcome.dirs, outcome.errors, outcome.excluded_dirs
    );
    if outcome.hardlink_inodes > 0 {
        print!(
            "  hardlinked inodes: {} (provisional first-seen totals)",
            outcome.hardlink_inodes
        );
    }
    println!();
    println!();
    println!("Top {} directories by real size:", cli.top);
    for dir in outcome.top_dirs_by_disk(cli.top) {
        let meta = outcome.dir(dir);
        println!(
            "  {:>10}  {}",
            HumanSize(meta.td).to_string(),
            outcome.path_of(dir).display()
        );
    }

    ExitCode::SUCCESS
}
