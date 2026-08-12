use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use cxs_core::{AppPaths, ProfileStore};

#[derive(Debug, Parser)]
#[command(about = "Run the local Codex Shuttle bridge", version)]
struct Arguments {
    /// Profile name to serve.
    profile: String,

    /// Codex CLI executable used to start app-server.
    #[arg(long, default_value = "codex")]
    codex: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let arguments = Arguments::parse();
    let store = ProfileStore::new(AppPaths::discover()?);
    let profile = store.load(&arguments.profile)?;
    cxs_bridge::serve(profile, store, arguments.codex).await
}
