use async_trait::async_trait;
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;

use crate::config::PluginConfig;

pub mod command_rewrite;

pub type ProxyBody = Full<Bytes>;
pub type ProxyRequest = Request<ProxyBody>;
pub type ProxyResponse = Response<ProxyBody>;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin {plugin} failed: {message}")]
    Failed {
        plugin: &'static str,
        message: String,
    },
}

#[async_trait]
pub trait TrafficPlugin: Send + Sync {
    fn name(&self) -> &'static str;

    fn requires_buffered_response(&self) -> bool {
        false
    }

    async fn on_request(&self, request: ProxyRequest) -> Result<ProxyRequest, PluginError> {
        Ok(request)
    }

    async fn on_response(&self, response: ProxyResponse) -> Result<ProxyResponse, PluginError> {
        Ok(response)
    }
}

pub type PluginStack = Vec<Box<dyn TrafficPlugin>>;

pub fn build_plugins(configs: &[PluginConfig]) -> anyhow::Result<PluginStack> {
    configs
        .iter()
        .map(|config| match config {
            PluginConfig::CommandRewrite(config) => {
                command_rewrite::CommandRewritePlugin::try_new(config.clone())
                    .map(|plugin| Box::new(plugin) as Box<dyn TrafficPlugin>)
            }
        })
        .collect()
}

pub async fn apply_request_plugins(
    plugins: &[Box<dyn TrafficPlugin>],
    mut request: ProxyRequest,
) -> Result<ProxyRequest, PluginError> {
    for plugin in plugins {
        request = plugin.on_request(request).await?;
    }

    Ok(request)
}

pub async fn apply_response_plugins(
    plugins: &[Box<dyn TrafficPlugin>],
    mut response: ProxyResponse,
) -> Result<ProxyResponse, PluginError> {
    for plugin in plugins.iter().rev() {
        response = plugin.on_response(response).await?;
    }

    Ok(response)
}

pub fn has_buffered_response_plugins(plugins: &[Box<dyn TrafficPlugin>]) -> bool {
    plugins
        .iter()
        .any(|plugin| plugin.requires_buffered_response())
}
