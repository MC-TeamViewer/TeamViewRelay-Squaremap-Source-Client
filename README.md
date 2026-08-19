# TeamViewRelay Squaremap Source Client

这个仓库是一个 Rust Cargo 包，提供两个二进制：

- `teamviewrelay-squaremap-source-client`：轮询 Squaremap `players.json`，通过 TeamViewRelay `/mc-client` WebSocket 上报玩家状态。
- `teamviewrelay-pass-cdn`：使用 headless Chromium 处理 EdgeOne 验证，并向主客户端提供稳定的本地 JSON 接口。

主客户端仍可直接读取源站。只有源站启用了浏览器验证且普通 HTTP 无法稳定访问时，才需要启用 `pass-cdn`。

## 运行要求

- Rust 1.94 或更高版本，或 Docker Compose
- `TeamViewRelay-Protocol` 的 `proto` 目录
- 使用本地 Cargo 构建时，将协议仓库放在本仓库同级目录，或者设置 `TEAMVIEWRELAY_PROTOCOL_DIR`
- 使用 Docker Compose 构建时，将协议仓库放在 `../TeamViewRelay-Protocol/proto`

## 直连源站

复制配置并修改 `relay_url`、`source_url` 和显示名称：

```bash
cp config.example.toml config.toml
```

本地运行：

```bash
cargo run --locked --release \
  --bin teamviewrelay-squaremap-source-client -- \
  --config config.toml
```

使用 Docker Compose：

```bash
docker compose up -d --build
docker compose logs -f squaremap-source
```

默认 Compose 不启动 Chromium，只运行静态 MUSL 构建的主客户端。主客户端镜像以 `scratch` 为运行层，没有 shell、包管理器或动态运行库。

## 使用 Chromium 处理 EdgeOne

复制 sidecar 配置，并修改其中的 `relay_url`。`source_url` 应保持为 Compose 内部地址 `http://pass-cdn:8080/tiles/players.json`：

```bash
cp config.pass-cdn.example.toml config.pass-cdn.toml
```

启动主客户端和 Rust sidecar：

```bash
TEAMVIEWRELAY_CONFIG_FILE=./config.pass-cdn.toml \
  docker compose --profile browser up -d --build
```

查看状态和日志：

```bash
docker compose --profile browser ps
docker compose --profile browser logs -f pass-cdn squaremap-source
```

检查 sidecar：

```bash
docker compose --profile browser exec pass-cdn \
  /usr/local/bin/teamviewrelay-pass-cdn \
  --healthcheck \
  --healthcheck-url http://127.0.0.1:8080/healthz
```

停止服务：

```bash
TEAMVIEWRELAY_CONFIG_FILE=./config.pass-cdn.toml \
  docker compose --profile browser down
```

sidecar 镜像包含 Rust 二进制和 Debian Chromium，不包含 Python。默认使用 `resident` 模式常驻一个浏览器会话。内存优先的服务器可以使用按需模式：

```bash
PASS_CDN_BROWSER_MODE=on-demand \
TEAMVIEWRELAY_CONFIG_FILE=./config.pass-cdn.toml \
  docker compose --profile browser up -d
```

按需模式先尝试普通 Rust HTTP 请求，遇到 EdgeOne 验证时才启动 Chromium。浏览器数据只写入容器 `/tmp`，容器重启后不会保留。

## 本地运行 pass-cdn

本地系统需要安装 Chromium、Chromium Browser 或 Google Chrome。程序会自动查找常见系统路径，也可以通过 `--browser-path` 明确指定。

一次性读取并输出 JSON：

```bash
cargo run --locked --release --bin teamviewrelay-pass-cdn -- --once
```

启动 HTTP sidecar：

```bash
cargo run --locked --release --bin teamviewrelay-pass-cdn -- \
  --serve --host 127.0.0.1 --port 8080
```

接口如下：

- `GET /tiles/players.json`：返回当前玩家 JSON，支持 `ETag` 和 `304 Not Modified`
- `GET /healthz`：缓存有效时返回 `200`，首次取数未完成或数据过期时返回 `503`

## 构建

构建两个 Rust 二进制：

```bash
cargo build --locked --release --bins
```

生成文件：

```text
target/release/teamviewrelay-squaremap-source-client
target/release/teamviewrelay-pass-cdn
```

构建 Compose 使用的两个镜像：

```bash
docker compose --profile browser build squaremap-source pass-cdn
```

如果 Docker Hub 不可达，可以覆盖基础镜像地址：

```bash
RUST_IMAGE=docker.1ms.run/library/rust:1.94.1-bookworm \
DEBIAN_IMAGE=docker.1ms.run/library/debian:bookworm-slim \
  docker compose --profile browser build squaremap-source pass-cdn
```

## 配置说明

主客户端的核心配置：

- `relay_url`：TeamViewRelay 玩家客户端端点，例如 `wss://relay.example.com/mc-client`
- `room_code`：Relay 房间，默认 `default`
- `source_url`：源站 `players.json` 或 sidecar 地址
- `source_id`：来源的稳定 UUID；切换直连和 sidecar 时应保持不变
- `poll_interval_secs`：轮询间隔
- `failure_grace_secs`：连续失败多久后清空本来源的实时状态
- `normalize_dimensions`：是否把 `minecraft_overworld` 等 Squaremap 世界键转换为 `minecraft:overworld`
- `history_state_path`：离线历史状态文件
- `history_retention_days`：离线历史保留天数，`0` 表示永久保留

Compose 默认把历史文件保存在 `squaremap-history` volume。主客户端限制为 `0.25 CPU`、`64MB` 内存；Chromium sidecar 限制为 `0.5 CPU`、`512MB` 内存。

## 故障语义

- `200` 响应更新玩家状态和位置采样时间。
- `304` 视为成功，只刷新当前名单的在线确认时间。
- 上游失败时保留旧状态；超过 `failure_grace_secs` 后清空本来源的实时玩家和 Tab 状态。
- EdgeOne 验证页会被识别为 `captcha_required`，不会作为 JSON 解析错误处理。
- 有 UUID 和名称但没有有效坐标的玩家仍会进入 Tab，但不会上报位置。
- 切换直连源站与 sidecar 时，保持相同 `source_id` 可延续实时状态和离线历史。

## 验证改动

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
docker compose config
TEAMVIEWRELAY_CONFIG_FILE=./config.pass-cdn.example.toml \
  docker compose --profile browser config
```

## 发布

推送 `v*` tag 后，GitHub Actions 构建 Linux x86_64 版本，并在同一个 artifact 中上传两个二进制。
