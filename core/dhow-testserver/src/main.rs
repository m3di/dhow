use std::io::Write;
use std::net::SocketAddr;

use clap::Parser;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(version, about = "Local HTTP server for Dhow tests")]
struct Args {
    /// Address to listen on; port 0 picks a free one.
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: SocketAddr,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind(args.bind).await?;
    println!("listening on http://{}", listener.local_addr()?);
    std::io::stdout().flush()?;
    dhow_testserver::serve(listener).await
}
