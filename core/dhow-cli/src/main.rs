use std::path::PathBuf;

use clap::Parser;
use dhow_core::logging;

#[derive(Parser)]
#[command(name = "dhow", version = dhow_core::VERSION, about = "Move files between any two shores")]
struct Cli {
    /// Log directory [default: the platform's log directory]
    #[arg(long, env = "DHOW_LOG_DIR", global = true)]
    log_dir: Option<PathBuf>,

    /// Log filter, in `tracing` EnvFilter syntax
    #[arg(long, env = "DHOW_LOG", default_value = logging::DEFAULT_FILTER, global = true)]
    log: String,
}

fn main() {
    let cli = Cli::parse();
    // Not being able to log must not stop a download.
    match cli.log_dir.or_else(logging::default_dir) {
        Some(dir) => {
            if let Err(e) = logging::init(&dir, &cli.log) {
                eprintln!("dhow: logging disabled: {e}");
            }
        }
        None => eprintln!("dhow: logging disabled: no log directory, pass --log-dir"),
    }
    tracing::info!(version = dhow_core::VERSION, "cli started");
}
