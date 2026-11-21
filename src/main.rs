//! The `qubo` command.

use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser as _;
use qubo_bridge::cli::{Cli, run};
use qubo_bridge::models::Paths;

fn main() -> anyhow::Result<ExitCode> {
    // Parse first, so that --help and usage errors do not start the runtime.
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    Ok(runtime.block_on(run(cli, &Paths::from_env())))
}
