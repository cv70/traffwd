# traffwd

`traffwd` 是一个使用 Rust 2024 编写的可编程网络流量代理应用。项目当前优先实现 HTTP 明文代理和 HTTPS `CONNECT` 隧道：它不仅转发请求，还在 HTTP 请求和响应路径上预留了可插拔的流量重写能力，便于后续扩展鉴权、观测、注入、脱敏等处理。

## 当前能力

- HTTP 代理监听本地地址并转发上游 `http://` 请求。
- 支持 HTTPS `CONNECT` 隧道，可作为系统或浏览器的 HTTPS 代理使用。
- 支持 CLI 指定监听地址，或从 TOML 配置文件加载运行参数。
- 未启用响应改写插件时，响应体会按上游 chunk/frame 流式透传；`Content-Type: text/event-stream` 的 SSE 响应即使启用了响应改写插件也会自动保持流式转发。
- 内置 `command_rewrite` 插件：用户配置外部命令，命令通过 stdin/stdout JSON 协议接收流量并返回重写结果。
- 请求插件按配置顺序执行，响应插件按相反顺序执行，方便形成类似中间件的处理栈。

## 运行方式

使用默认配置运行，默认监听 `127.0.0.1:8080`：

```sh
cargo run
```

显式指定监听地址：

```sh
cargo run -- --listen 127.0.0.1:8080
```

加载示例配置：

```sh
cargo run -- --config examples/command-rewrite.toml
```

通过代理发起 HTTP 请求：

```sh
curl -x http://127.0.0.1:8080 http://example.com/
```

也可以使用环境变量提供参数：

```sh
TRAFFWD_LISTEN=127.0.0.1:8080 cargo run
TRAFFWD_CONFIG=examples/command-rewrite.toml cargo run
```

## 配置系统代理

先启动 `traffwd`：

```sh
cargo run -- --config examples/command-rewrite.toml
```

HTTP/Web Proxy 和 HTTPS/Secure Web Proxy 都可以指向 `traffwd`。HTTPS 流量通过 `CONNECT` 建立 TCP 隧道转发；当前不会解密 TLS，也不会对隧道内的 HTTPS 请求/响应执行插件改写。

### macOS

图形界面：

1. 打开 System Settings。
2. 进入 Network。
3. 选择当前网络服务，例如 Wi-Fi。
4. 进入 Details → Proxies。
5. 开启 Web Proxy (HTTP)，地址填 `127.0.0.1`，端口填 `8080`。
6. 开启 Secure Web Proxy (HTTPS)，地址同样填 `127.0.0.1`，端口填 `8080`。

命令行方式：

```sh
networksetup -listallnetworkservices
networksetup -setwebproxy "Wi-Fi" 127.0.0.1 8080
networksetup -setwebproxystate "Wi-Fi" on
networksetup -setsecurewebproxy "Wi-Fi" 127.0.0.1 8080
networksetup -setsecurewebproxystate "Wi-Fi" on
```

关闭代理：

```sh
networksetup -setwebproxystate "Wi-Fi" off
networksetup -setsecurewebproxystate "Wi-Fi" off
```

如果你的网络服务名称不是 `Wi-Fi`，把命令里的 `Wi-Fi` 换成 `networksetup -listallnetworkservices` 输出的名称。

### Windows

图形界面：

1. 打开 Settings。
2. 进入 Network & Internet → Proxy。
3. 开启 Manual proxy setup。
4. Address 填 `127.0.0.1`，Port 填 `8080`。
5. 保存设置。

HTTPS 流量会通过 `CONNECT` 隧道转发；traffwd 不解密 TLS，因此 `command_rewrite` 只作用于普通 HTTP 请求/响应，不作用于隧道内的 HTTPS 内容。

### Linux / GNOME

图形界面：

1. 打开 Settings。
2. 进入 Network → Network Proxy。
3. 选择 Manual。
4. HTTP Proxy 填 `127.0.0.1`，端口填 `8080`。
5. HTTPS Proxy 填 `127.0.0.1`，端口填 `8080`。

### 命令行工具

如果只想让当前 shell 里的命令走代理，可以使用环境变量：

```sh
export HTTP_PROXY=http://127.0.0.1:8080
export http_proxy=http://127.0.0.1:8080
export HTTPS_PROXY=http://127.0.0.1:8080
export https_proxy=http://127.0.0.1:8080
```

关闭当前 shell 的代理环境变量：

```sh
unset HTTP_PROXY http_proxy HTTPS_PROXY https_proxy
```

## 配置示例

最小配置只需要指定监听地址；不配置插件时，代理只执行基础转发：

```toml
listen = "127.0.0.1:8080"
plugins = []
```

启用 `command_rewrite` 插件，把重写逻辑放到外部命令中：

```toml
listen = "127.0.0.1:8080"

[[plugins]]
type = "command_rewrite"

[plugins.request]
program = "python3"
args = ["examples/rewriters/header_marker.py", "request"]
timeout_ms = 1000

[plugins.response]
program = "python3"
args = ["examples/rewriters/header_marker.py", "response"]
timeout_ms = 1000
```

完整示例见 `examples/command-rewrite.toml`。

## 命令重写协议

`command_rewrite` 不通过 shell 执行命令，而是直接执行 `program` 并传入 `args`。每次请求或响应都会启动一次命令进程：

- traffwd 向命令 stdin 写入一个 JSON 对象。
- 命令向 stdout 写回一个 JSON 对象。
- 命令退出码必须为 `0`。
- 命令必须在 `timeout_ms` 内完成。
- stderr 只在命令失败时作为错误信息记录。

请求阶段输入：

```json
{
  "version": 1,
  "phase": "request",
  "request": {
    "method": "GET",
    "uri": "http://example.com/",
    "headers": {
      "host": ["example.com"]
    },
    "body_base64": ""
  },
  "response": null
}
```

请求阶段输出可以只返回需要覆盖的字段：

```json
{
  "version": 1,
  "request": {
    "method": "POST",
    "headers": {
      "host": ["example.com"],
      "x-added-by-command": ["true"]
    },
    "body_base64": "cmV3cml0dGVu"
  }
}
```

响应阶段输入：

```json
{
  "version": 1,
  "phase": "response",
  "request": null,
  "response": {
    "status": 200,
    "headers": {
      "content-type": ["text/plain"]
    },
    "body_base64": "aGVsbG8="
  }
}
```

响应阶段输出：

```json
{
  "version": 1,
  "response": {
    "status": 201,
    "headers": {
      "content-type": ["text/plain"],
      "x-added-by-command": ["true"]
    },
    "body_base64": "cmV3cml0dGVu"
  }
}
```

输出字段语义：

- `method`、`uri`、`status`、`body_base64` 都是可选覆盖字段。
- `headers` 是可选字段；一旦返回，会整体替换对应请求或响应的 headers。
- `headers` 使用 `{ "header-name": ["value1", "value2"] }` 形式，而不是单值 map，因为 HTTP 允许同名 header 多次出现。
- 不需要修改时可以返回 `{"version": 1}`。
- body 统一使用 base64，避免文本编码破坏二进制内容。

## 架构扩展点

- `AppConfig` 负责加载全局监听地址和插件配置。
- `PluginConfig` 描述可用插件类型，当前主扩展路径是 `command_rewrite`。
- `TrafficPlugin` 是流量处理扩展点，插件可实现 `on_request` 和 `on_response`。
- `build_plugins` 将配置转换为运行时插件栈。
- `HttpProxy` 负责连接监听、请求归一化、HTTP 上游转发、HTTPS `CONNECT` 隧道、响应收集，以及在请求/响应路径上调用插件栈。

## 限制

- 当前支持 HTTP 明文代理和 HTTPS `CONNECT` 隧道；HTTPS 隧道只做 TCP 转发，不处理 TLS 终止，也不读取或改写隧道内流量。
- 当前会收集完整请求体后再转发；启用响应改写插件时也会收集完整响应体，因此这类配置不适合超大响应。标准 `Content-Type: text/event-stream` 的 SSE 响应会自动跳过响应改写插件并保持流式转发。
- `command_rewrite` 当前每次请求/响应都会启动一个进程，适合先验证协议和能力；高吞吐场景后续应扩展为长驻进程或本地 RPC。
- 当前 header 协议要求 header value 可表示为 UTF-8 字符串；二进制 header value 暂不支持。
