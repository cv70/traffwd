use std::{net::SocketAddr, sync::Arc};

use axum::{Router, body::Body, extract::State, response::IntoResponse, routing::any};
use bytes::Bytes;
use http::{
    Method, Request, Response, StatusCode, Uri,
    header::{CONTENT_TYPE, HOST, HeaderValue},
};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, upgrade::Upgraded};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use tokio::{io, net::TcpListener, net::TcpStream, signal};
use tracing::{debug, error, info};
use url::Url;

use crate::plugins::{
    PluginStack, ProxyBody, ProxyRequest, ProxyResponse, apply_request_plugins,
    apply_response_plugins, has_buffered_response_plugins,
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
    ) -> anyhow::Result<Response<Body>> {
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
        let response =
            if has_buffered_response_plugins(&self.plugins) && !is_sse_response(&response) {
                let response = collect_response(response).await?;
                apply_response_plugins(&self.plugins, response)
                    .await?
                    .map(Body::new)
            } else {
                response.map(Body::new)
            };

        Ok(response)
    }
}

async fn handle_proxy_request(
    State(proxy): State<Arc<HttpProxy>>,
    axum::extract::ConnectInfo(remote_addr): axum::extract::ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> impl IntoResponse {
    if request.method() == Method::CONNECT {
        return handle_connect_request(remote_addr, request).await;
    }

    match proxy.forward(request, remote_addr).await {
        Ok(response) => response,
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

async fn handle_connect_request(remote_addr: SocketAddr, request: Request<Body>) -> Response<Body> {
    let authority = match connect_authority(request.uri()) {
        Ok(authority) => authority,
        Err(error) => {
            error!(%remote_addr, %error, "invalid CONNECT request");
            return text_response(
                StatusCode::BAD_REQUEST,
                format!("invalid CONNECT target: {error}\n"),
            )
            .map(Body::new);
        }
    };

    info!(%remote_addr, target = %authority, "opening CONNECT tunnel");

    let server = match TcpStream::connect(&authority).await {
        Ok(server) => server,
        Err(error) => {
            error!(%remote_addr, target = %authority, %error, "failed to connect CONNECT target");
            return text_response(
                StatusCode::BAD_GATEWAY,
                format!("failed to connect CONNECT target: {error}\n"),
            )
            .map(Body::new);
        }
    };

    tokio::spawn(async move {
        match hyper::upgrade::on(request).await {
            Ok(upgraded) => {
                if let Err(error) = tunnel(upgraded, server, &authority).await {
                    error!(%remote_addr, target = %authority, %error, "CONNECT tunnel failed");
                }
            }
            Err(error) => {
                error!(%remote_addr, target = %authority, %error, "failed to upgrade CONNECT request");
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .expect("CONNECT response should be valid")
}

fn connect_authority(uri: &Uri) -> anyhow::Result<String> {
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("CONNECT target is missing authority"))?;

    if authority.host().is_empty() {
        anyhow::bail!("CONNECT target host is empty");
    }

    if authority.port_u16().is_none() {
        anyhow::bail!("CONNECT target must include a port");
    }

    Ok(authority.as_str().to_owned())
}

async fn tunnel(upgraded: Upgraded, mut server: TcpStream, authority: &str) -> anyhow::Result<()> {
    let mut upgraded = TokioIo::new(upgraded);
    let (from_client, from_server) = io::copy_bidirectional(&mut upgraded, &mut server).await?;
    debug!(%authority, from_client, from_server, "CONNECT tunnel closed");
    Ok(())
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

fn is_sse_response(response: &Response<Incoming>) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("text/event-stream")
            })
        })
}

fn text_response(status: StatusCode, text: String) -> ProxyResponse {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(text)))
        .expect("static response should be valid")
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use axum::{Router, body::Body, routing::any};
    use http::Request;
    use http_body_util::BodyExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        time::timeout,
    };

    use crate::plugins::{PluginError, ProxyResponse, TrafficPlugin};

    use super::{HttpProxy, handle_proxy_request};

    struct ResponsePlugin;

    #[async_trait]
    impl TrafficPlugin for ResponsePlugin {
        fn name(&self) -> &'static str {
            "response_plugin"
        }

        fn requires_buffered_response(&self) -> bool {
            true
        }

        async fn on_response(
            &self,
            mut response: ProxyResponse,
        ) -> Result<ProxyResponse, PluginError> {
            response.headers_mut().insert(
                "x-response-plugin",
                "applied".parse().expect("test header value should parse"),
            );
            Ok(response)
        }
    }

    #[tokio::test]
    async fn forwards_response_body_without_waiting_for_complete_upstream_body() {
        assert_first_sse_frame_arrives_before_upstream_completes(
            HttpProxy::new(Vec::new()),
            "text/event-stream",
        )
        .await;
    }

    #[tokio::test]
    async fn streams_sse_response_even_when_response_plugins_are_configured() {
        assert_first_sse_frame_arrives_before_upstream_completes(
            HttpProxy::new(vec![Box::new(ResponsePlugin)]),
            "text/event-stream",
        )
        .await;
    }

    #[tokio::test]
    async fn recognizes_sse_content_type_case_insensitively_with_parameters() {
        assert_first_sse_frame_arrives_before_upstream_completes(
            HttpProxy::new(vec![Box::new(ResponsePlugin)]),
            "Text/Event-Stream; charset=utf-8",
        )
        .await;
    }

    #[tokio::test]
    async fn connect_tunnels_bytes_between_client_and_target() {
        let target_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("target listener should bind");
        let target_addr = target_listener
            .local_addr()
            .expect("target listener address should be available");

        let target_task = tokio::spawn(async move {
            let (mut socket, _) = target_listener
                .accept()
                .await
                .expect("target should accept tunnel connection");
            let mut payload = [0; 4];
            socket
                .read_exact(&mut payload)
                .await
                .expect("target should read tunneled bytes");
            assert_eq!(&payload, b"ping");
            socket
                .write_all(b"pong")
                .await
                .expect("target should write tunneled bytes");
        });

        let proxy_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy listener should bind");
        let proxy_addr = proxy_listener
            .local_addr()
            .expect("proxy listener address should be available");
        let app = Router::new()
            .fallback(any(handle_proxy_request))
            .with_state(Arc::new(HttpProxy::new(Vec::new())));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let proxy_task = tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("proxy should serve test request");
        });

        let mut client = TcpStream::connect(proxy_addr)
            .await
            .expect("client should connect to proxy");
        client
            .write_all(
                format!("CONNECT {target_addr} HTTP/1.1\r\nhost: {target_addr}\r\n\r\n").as_bytes(),
            )
            .await
            .expect("client should write CONNECT request");

        let mut response = Vec::new();
        loop {
            let mut byte = [0];
            client
                .read_exact(&mut byte)
                .await
                .expect("client should read CONNECT response");
            response.push(byte[0]);
            if response.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let response = String::from_utf8(response).expect("response should be utf-8");
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected CONNECT response: {response:?}"
        );

        client
            .write_all(b"ping")
            .await
            .expect("client should write tunneled bytes");
        let mut tunneled = [0; 4];
        client
            .read_exact(&mut tunneled)
            .await
            .expect("client should read tunneled bytes");
        assert_eq!(&tunneled, b"pong");

        let _ = shutdown_tx.send(());
        proxy_task.await.expect("proxy task should finish");
        target_task.await.expect("target task should finish");
    }

    async fn assert_first_sse_frame_arrives_before_upstream_completes(
        proxy: HttpProxy,
        content_type: &'static str,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let upstream_addr = listener
            .local_addr()
            .expect("upstream listener address should be available");
        let (finish_tx, finish_rx) = oneshot::channel();

        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("upstream should accept proxy connection");
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let bytes_read = socket
                    .read(&mut buffer)
                    .await
                    .expect("upstream should read request");
                if bytes_read == 0 {
                    break;
                }

                request.extend_from_slice(&buffer[..bytes_read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            let response_head = format!(
                "HTTP/1.1 200 OK\r\n\
                 content-type: {content_type}\r\n\
                 transfer-encoding: chunked\r\n\
                 \r\n\
                 d\r\n\
                 data: first\n\n\r\n"
            );
            socket
                .write_all(response_head.as_bytes())
                .await
                .expect("upstream should write first chunk");
            socket
                .flush()
                .await
                .expect("upstream should flush first chunk");

            let _ = finish_rx.await;
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });

        let response = proxy
            .forward(
                Request::builder()
                    .uri(format!("http://{upstream_addr}/events"))
                    .body(Body::empty())
                    .expect("request should build"),
                "127.0.0.1:50000"
                    .parse()
                    .expect("remote address should parse"),
            )
            .await
            .expect("proxy should forward request");

        let mut body = response.into_body();
        let frame = timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("first response frame should arrive before upstream completes")
            .expect("response body should yield a frame")
            .expect("first response frame should be valid");
        let data = frame
            .into_data()
            .expect("first response frame should contain data");

        assert_eq!(data, "data: first\n\n");

        let _ = finish_tx.send(());
        upstream_task.await.expect("upstream task should finish");
    }
}
