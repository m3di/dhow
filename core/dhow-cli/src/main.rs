use clap::Parser;

#[derive(Parser)]
#[command(name = "dhow", version = dhow_core::VERSION, about = "Move files between any two shores")]
struct Cli {}

fn main() {
    Cli::parse();
}
