use std::{net::SocketAddr, path::Path};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub listen: SocketAddr,
    pub plugins: Vec<PluginConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginConfig {
    CommandRewrite(CommandRewriteConfig),
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandRewriteConfig {
    #[serde(default)]
    pub request: Option<CommandConfig>,
    #[serde(default)]
    pub response: Option<CommandConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_command_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            plugins: Vec::new(),
        }
    }
}

impl AppConfig {
    pub async fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = tokio::fs::read_to_string(path).await?;
        let config = toml::from_str(&content)?;
        Ok(config)
    }
}

fn default_command_timeout_ms() -> u64 {
    1000
}
