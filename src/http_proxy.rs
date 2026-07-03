use std::{net::SocketAddr, sync::Arc};

use axum::{Router, body::Body, extract::State, response::IntoResponse, routing::any};
use bytes::Bytes;
use http::{
    Request, Response, StatusCode, Uri,
    header::{HOST, HeaderValue},
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use tokio::{net::TcpListener, signal};
use tracing::{error, info};
use url::Url;

use crate::plugins::{
    PluginStack, ProxyBody, ProxyRequest, ProxyResponse, apply_request_plugins,
    apply_response_plugins,
};

#[derive(Clone)]
pub struct HttpProxy {
    client: Client<HttpConnector, ProxyBody>,
    plugins: Arc<PluginStack>,
}

impl HttpProxy {
    pub fn new(plugins: PluginStack) -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);

        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            plugins: Arc::new(plugins),
        }
    }

    pub async fn serve(self, listen: SocketAddr) -> anyhow::Result<()> {
        let listener = TcpListener::bind(listen).await?;
        let app = Router::new()
            .fallback(any(handle_proxy_request))
            .with_state(Arc::new(self));

        info!(%listen, "http proxy listening");

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await?;

        Ok(())
    }

    async fn forward(
        &self,
        request: Request<Body>,
        remote_addr: SocketAddr,
    ) -> anyhow::Result<ProxyResponse> {
        let request = normalize_request(request).await?;
        let request = apply_request_plugins(&self.plugins, request).await?;
        let outbound_uri = request.uri().clone();

        info!(
            %remote_addr,
            method = %request.method(),
            uri = %outbound_uri,
            "forwarding request"
        );

        let response = self.client.request(request).await?;
        let response = collect_response(response).await?;
        let response = apply_response_plugins(&self.plugins, response).await?;

        Ok(response)
    }
}

async fn handle_proxy_request(
    State(proxy): State<Arc<HttpProxy>>,
    axum::extract::ConnectInfo(remote_addr): axum::extract::ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> impl IntoResponse {
    match proxy.forward(request, remote_addr).await {
        Ok(response) => response.map(Body::new),
        Err(error) => {
            error!(%remote_addr, %error, "request failed");
            text_response(
                StatusCode::BAD_GATEWAY,
                format!("proxy request failed: {error}\n"),
            )
            .map(Body::new)
        }
    }
}

async fn shutdown_signal() {
    if let Err(error) = signal::ctrl_c().await {
        error!(%error, "failed to listen for shutdown signal");
    }
    info!("shutdown signal received");
}

async fn normalize_request(request: Request<Body>) -> anyhow::Result<ProxyRequest> {
    let (mut parts, body) = request.into_parts();
    let body = body.collect().await?.to_bytes();

    let target_uri = match parts.uri.scheme() {
        Some(_) => parts.uri.clone(),
        None => absolute_uri_from_origin_form(&parts.uri, parts.headers.get(HOST))?,
    };

    if target_uri.scheme_str() != Some("http") {
        anyhow::bail!("only http upstream requests are supported for now");
    }

    let authority = target_uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("request target is missing authority"))?
        .clone();

    parts
        .headers
        .insert(HOST, HeaderValue::from_str(authority.as_str())?);
    parts.uri = target_uri;

    Ok(Request::from_parts(parts, Full::new(body)))
}

fn absolute_uri_from_origin_form(uri: &Uri, host: Option<&HeaderValue>) -> anyhow::Result<Uri> {
    let host = host.ok_or_else(|| anyhow::anyhow!("origin-form request is missing Host header"))?;
    let host = host.to_str()?;
    let path = uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/");
    let url = Url::parse(&format!("http://{host}{path}"))?;
    Ok(url.as_str().parse()?)
}

async fn collect_response(response: Response<Incoming>) -> anyhow::Result<ProxyResponse> {
    let (parts, body) = response.into_parts();
    let body = body.collect().await?.to_bytes();
    Ok(Response::from_parts(parts, Full::new(body)))
}

fn text_response(status: StatusCode, text: String) -> ProxyResponse {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(text)))
        .expect("static response should be valid")
}
