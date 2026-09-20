mod cli;
mod config;
mod daemon;
mod protocol;
mod provider;
mod scheduler;
mod store;

use clap::Parser;

#[tokio::main]
async fn main() {
    let args = cli::Cli::parse();
    if let Err(error) = cli::run(args).await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
