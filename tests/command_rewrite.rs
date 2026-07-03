use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use http::{Request, Response};
use traffwd::{
    config::{AppConfig, PluginConfig},
    plugins::{ProxyBody, apply_request_plugins, apply_response_plugins, build_plugins},
};

fn empty_body() -> ProxyBody {
    ProxyBody::new(bytes::Bytes::new())
}

fn executable_script(path: &Path, content: &str) {
    fs::write(path, content).expect("script should be written");
    let mut permissions = fs::metadata(path)
        .expect("script metadata should be readable")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script should be executable");
}

#[test]
fn parses_toml_config_with_command_rewrite_plugin() {
    let config: AppConfig = toml::from_str(
        r#"
listen = "127.0.0.1:18080"

[[plugins]]
type = "command_rewrite"

[plugins.request]
program = "request-rewriter"
args = ["--request"]
timeout_ms = 1500

[plugins.response]
program = "response-rewriter"
"#,
    )
    .expect("valid TOML config should deserialize");

    assert_eq!(config.listen.to_string(), "127.0.0.1:18080");
    assert_eq!(config.plugins.len(), 1);

    let PluginConfig::CommandRewrite(command_rewrite) = &config.plugins[0];
    let request = command_rewrite
        .request
        .as_ref()
        .expect("request command should be configured");
    let response = command_rewrite
        .response
        .as_ref()
        .expect("response command should be configured");

    assert_eq!(request.program, "request-rewriter");
    assert_eq!(request.args, ["--request"]);
    assert_eq!(request.timeout_ms, 1500);
    assert_eq!(response.program, "response-rewriter");
    assert_eq!(response.timeout_ms, 1000);
}

#[test]
fn build_plugins_rejects_empty_command_rewrite() {
    let config: AppConfig = toml::from_str(
        r#"
[[plugins]]
type = "command_rewrite"
"#,
    )
    .expect("empty command rewrite config still deserializes");

    let error = match build_plugins(&config.plugins) {
        Ok(_) => panic!("empty command rewrite should fail to build"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("requires at least one request or response command"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn command_rewrite_can_leave_request_unchanged() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let script = temp_dir.path().join("noop.sh");
    executable_script(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"version":1}'
"#,
    );

    let config: AppConfig = toml::from_str(&format!(
        r#"
[[plugins]]
type = "command_rewrite"

[plugins.request]
program = "{}"
timeout_ms = 5000
"#,
        script.display()
    ))
    .expect("command rewrite config should deserialize");
    let plugins = build_plugins(&config.plugins).expect("plugins should build");

    let request = Request::builder()
        .method("GET")
        .uri("http://example.test/")
        .header("x-original", "kept")
        .body(empty_body())
        .expect("request should build");

    let request = apply_request_plugins(&plugins, request)
        .await
        .expect("request command should apply");

    assert_eq!(request.method(), "GET");
    assert_eq!(request.headers()["x-original"], "kept");
}

#[tokio::test]
async fn command_rewrite_can_patch_request_headers_and_body() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let script = temp_dir.path().join("rewrite-request.sh");
    executable_script(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"version":1,"request":{"method":"POST","headers":{"x-command":["request"]},"body_base64":"cmV3cml0dGVuLXJlcXVlc3Q="}}'
"#,
    );

    let config: AppConfig = toml::from_str(&format!(
        r#"
[[plugins]]
type = "command_rewrite"

[plugins.request]
program = "{}"
timeout_ms = 5000
"#,
        script.display()
    ))
    .expect("command rewrite config should deserialize");
    let plugins = build_plugins(&config.plugins).expect("plugins should build");

    let request = Request::builder()
        .method("GET")
        .uri("http://example.test/")
        .header("x-original", "removed")
        .body(ProxyBody::new(bytes::Bytes::from_static(b"original")))
        .expect("request should build");

    let request = apply_request_plugins(&plugins, request)
        .await
        .expect("request command should apply");
    let (parts, body) = request.into_parts();
    let body = http_body_util::BodyExt::collect(body)
        .await
        .expect("body should collect")
        .to_bytes();

    assert_eq!(parts.method, "POST");
    assert_eq!(parts.headers["x-command"], "request");
    assert!(!parts.headers.contains_key("x-original"));
    assert_eq!(body, "rewritten-request");
}

#[tokio::test]
async fn command_rewrite_can_patch_response_status_headers_and_body() {
    let temp_dir = tempfile::tempdir().expect("temp dir should be created");
    let script = temp_dir.path().join("rewrite-response.sh");
    executable_script(
        &script,
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"version":1,"response":{"status":201,"headers":{"x-command":["response"]},"body_base64":"cmV3cml0dGVuLXJlc3BvbnNl"}}'
"#,
    );

    let config: AppConfig = toml::from_str(&format!(
        r#"
[[plugins]]
type = "command_rewrite"

[plugins.response]
program = "{}"
timeout_ms = 5000
"#,
        script.display()
    ))
    .expect("command rewrite config should deserialize");
    let plugins = build_plugins(&config.plugins).expect("plugins should build");

    let response = Response::builder()
        .status(200)
        .header("x-original", "removed")
        .body(ProxyBody::new(bytes::Bytes::from_static(b"original")))
        .expect("response should build");

    let response = apply_response_plugins(&plugins, response)
        .await
        .expect("response command should apply");
    let (parts, body) = response.into_parts();
    let body = http_body_util::BodyExt::collect(body)
        .await
        .expect("body should collect")
        .to_bytes();

    assert_eq!(parts.status, 201);
    assert_eq!(parts.headers["x-command"], "response");
    assert!(!parts.headers.contains_key("x-original"));
    assert_eq!(body, "rewritten-response");
}
