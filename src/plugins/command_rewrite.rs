use std::{collections::BTreeMap, process::Stdio, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

use crate::{
    config::{CommandConfig, CommandRewriteConfig},
    plugins::{PluginError, ProxyRequest, ProxyResponse, TrafficPlugin},
};

const PLUGIN_NAME: &str = "command_rewrite";
const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone)]
pub struct CommandRewritePlugin {
    request: Option<CommandRunner>,
    response: Option<CommandRunner>,
}

#[derive(Debug, Clone)]
struct CommandRunner {
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

#[derive(Debug, Serialize)]
struct CommandInput {
    version: u8,
    phase: CommandPhase,
    request: Option<CommandRequest>,
    response: Option<CommandResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum CommandPhase {
    Request,
    Response,
}

#[derive(Debug, Serialize)]
struct CommandRequest {
    method: String,
    uri: String,
    headers: CommandHeaders,
    body_base64: String,
}

#[derive(Debug, Serialize)]
struct CommandResponse {
    status: u16,
    headers: CommandHeaders,
    body_base64: String,
}

type CommandHeaders = BTreeMap<String, Vec<String>>;

#[derive(Debug, Deserialize)]
struct CommandOutput {
    version: Option<u8>,
    request: Option<CommandRequestPatch>,
    response: Option<CommandResponsePatch>,
}

#[derive(Debug, Deserialize)]
struct CommandRequestPatch {
    method: Option<String>,
    uri: Option<String>,
    headers: Option<CommandHeaders>,
    body_base64: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommandResponsePatch {
    status: Option<u16>,
    headers: Option<CommandHeaders>,
    body_base64: Option<String>,
}

impl CommandRewritePlugin {
    pub fn try_new(config: CommandRewriteConfig) -> anyhow::Result<Self> {
        if config.request.is_none() && config.response.is_none() {
            anyhow::bail!("command_rewrite requires at least one request or response command");
        }

        Ok(Self {
            request: config.request.map(CommandRunner::try_new).transpose()?,
            response: config.response.map(CommandRunner::try_new).transpose()?,
        })
    }
}

#[async_trait]
impl TrafficPlugin for CommandRewritePlugin {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    async fn on_request(&self, request: ProxyRequest) -> Result<ProxyRequest, PluginError> {
        let Some(runner) = &self.request else {
            return Ok(request);
        };

        rewrite_request(runner, request).await
    }

    async fn on_response(&self, response: ProxyResponse) -> Result<ProxyResponse, PluginError> {
        let Some(runner) = &self.response else {
            return Ok(response);
        };

        rewrite_response(runner, response).await
    }
}

impl CommandRunner {
    fn try_new(config: CommandConfig) -> anyhow::Result<Self> {
        if config.program.trim().is_empty() {
            anyhow::bail!("command_rewrite program cannot be empty");
        }

        if config.timeout_ms == 0 {
            anyhow::bail!("command_rewrite timeout_ms must be greater than zero");
        }

        Ok(Self {
            program: config.program,
            args: config.args,
            timeout: Duration::from_millis(config.timeout_ms),
        })
    }

    async fn execute(&self, input: &CommandInput) -> Result<CommandOutput, PluginError> {
        let stdin_payload = serde_json::to_vec(input)
            .map_err(|error| failed(format!("failed to encode command input: {error}")))?;

        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| failed(format!("failed to spawn command: {error}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&stdin_payload)
                .await
                .map_err(|error| failed(format!("failed to write command stdin: {error}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|error| failed(format!("failed to close command stdin: {error}")))?;
        } else {
            return Err(failed("failed to open command stdin"));
        }

        let output = timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| failed("command timed out"))?
            .map_err(|error| failed(format!("failed to wait for command: {error}")))?;

        if !output.status.success() {
            return Err(failed(format!(
                "command exited with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        if output.stdout.is_empty() {
            return Err(failed(
                "command stdout is empty; expected JSON rewrite result",
            ));
        }

        let output: CommandOutput = serde_json::from_slice(&output.stdout)
            .map_err(|error| failed(format!("failed to decode command stdout as JSON: {error}")))?;

        if let Some(version) = output.version
            && version != PROTOCOL_VERSION
        {
            return Err(failed(format!(
                "unsupported command output protocol version: {version}"
            )));
        }

        Ok(output)
    }
}

async fn rewrite_request(
    runner: &CommandRunner,
    request: ProxyRequest,
) -> Result<ProxyRequest, PluginError> {
    let (mut parts, body) = request.into_parts();
    let mut body = collect_full_body(body).await?;
    let input = CommandInput {
        version: PROTOCOL_VERSION,
        phase: CommandPhase::Request,
        request: Some(CommandRequest {
            method: parts.method.to_string(),
            uri: parts.uri.to_string(),
            headers: headers_to_wire(&parts.headers)?,
            body_base64: STANDARD.encode(&body),
        }),
        response: None,
    };

    let output = runner.execute(&input).await?;
    if let Some(patch) = output.request {
        if let Some(method) = patch.method {
            parts.method = method
                .parse::<Method>()
                .map_err(|error| failed(format!("invalid rewritten request method: {error}")))?;
        }

        if let Some(uri) = patch.uri {
            parts.uri = uri
                .parse::<Uri>()
                .map_err(|error| failed(format!("invalid rewritten request uri: {error}")))?;
        }

        if let Some(headers) = patch.headers {
            parts.headers = headers_from_wire(headers)?;
        }

        if let Some(body_base64) = patch.body_base64 {
            body = STANDARD
                .decode(body_base64)
                .map_err(|error| failed(format!("invalid rewritten request body_base64: {error}")))?
                .into();
        }
    }

    Ok(ProxyRequest::from_parts(parts, Full::new(body)))
}

async fn rewrite_response(
    runner: &CommandRunner,
    response: ProxyResponse,
) -> Result<ProxyResponse, PluginError> {
    let (mut parts, body) = response.into_parts();
    let mut body = collect_full_body(body).await?;
    let input = CommandInput {
        version: PROTOCOL_VERSION,
        phase: CommandPhase::Response,
        request: None,
        response: Some(CommandResponse {
            status: parts.status.as_u16(),
            headers: headers_to_wire(&parts.headers)?,
            body_base64: STANDARD.encode(&body),
        }),
    };

    let output = runner.execute(&input).await?;
    if let Some(patch) = output.response {
        if let Some(status) = patch.status {
            parts.status = StatusCode::from_u16(status)
                .map_err(|error| failed(format!("invalid rewritten response status: {error}")))?;
        }

        if let Some(headers) = patch.headers {
            parts.headers = headers_from_wire(headers)?;
        }

        if let Some(body_base64) = patch.body_base64 {
            body = STANDARD
                .decode(body_base64)
                .map_err(|error| {
                    failed(format!("invalid rewritten response body_base64: {error}"))
                })?
                .into();
        }
    }

    Ok(ProxyResponse::from_parts(parts, Full::new(body)))
}

async fn collect_full_body(body: Full<Bytes>) -> Result<Bytes, PluginError> {
    body.collect()
        .await
        .map_err(|never| match never {})
        .map(|collected| collected.to_bytes())
}

fn headers_to_wire(headers: &HeaderMap) -> Result<CommandHeaders, PluginError> {
    let mut wire_headers = BTreeMap::new();

    for (name, value) in headers {
        let value = value
            .to_str()
            .map_err(|error| failed(format!("header {name} is not valid UTF-8: {error}")))?;

        wire_headers
            .entry(name.as_str().to_owned())
            .or_insert_with(Vec::new)
            .push(value.to_owned());
    }

    Ok(wire_headers)
}

fn headers_from_wire(headers: CommandHeaders) -> Result<HeaderMap, PluginError> {
    let mut rewritten = HeaderMap::new();

    for (name, values) in headers {
        let name = name
            .parse::<HeaderName>()
            .map_err(|error| failed(format!("invalid rewritten header name: {error}")))?;

        for value in values {
            let value = HeaderValue::from_str(&value)
                .map_err(|error| failed(format!("invalid rewritten header value: {error}")))?;
            rewritten.append(name.clone(), value);
        }
    }

    Ok(rewritten)
}

fn failed(message: impl Into<String>) -> PluginError {
    PluginError::Failed {
        plugin: PLUGIN_NAME,
        message: message.into(),
    }
}
