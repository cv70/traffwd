use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};
use traffwd::{config::AppConfig, http_proxy::HttpProxy, plugins::build_plugins};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(short, long, env = "TRAFFWD_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, env = "TRAFFWD_LISTEN")]
    listen: Option<std::net::SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut config = match cli.config {
        Some(path) => AppConfig::load(path).await?,
        None => AppConfig::default(),
    };

    if let Some(listen) = cli.listen {
        config.listen = listen;
    }

    let plugins = build_plugins(&config.plugins)?;
    HttpProxy::new(plugins).serve(config.listen).await
}
