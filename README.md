# TeamViewRelay Squaremap Source Client

轻量 Rust 常驻客户端，轮询 Squaremap 的 `players.json` 接口，并通过 TeamViewRelay 现有的 `/mc-client` WebSocket 上报。

客户端使用协议 `0.6.4` 的 `CLIENT_ROLE_EXTERNAL_SOURCE` 角色。`relay_url` 和 `source_url` 都是必填项；房间默认使用 `default`，默认每 5 秒轮询一次，并在上游连续失败 30 秒后清空本来源的实时状态。

`relay_url` 必须包含玩家客户端端点，例如 `wss://relay.example.com/mc-client`。客户端直接向该地址发送现有的 `PlayerHandshakeRequest`，不使用 `/web-map/ws`，也不需要后端新增路由。

## 运行

要求 Rust 1.94+，并将 `TeamViewRelay-Protocol` checkout 在本仓库同级目录，或设置 `TEAMVIEWRELAY_PROTOCOL_DIR` 指向其 `proto` 目录。

```bash
cp config.example.toml config.toml
cargo run --release -- --config config.toml
```

也可以使用环境变量覆盖配置：`TEAMVIEWRELAY_RELAY_URL`、`TEAMVIEWRELAY_ROOM_CODE`、`TEAMVIEWRELAY_SOURCE_URL`、`TEAMVIEWRELAY_SOURCE_COOKIE_FILE`、`TEAMVIEWRELAY_SOURCE_USER_AGENT`、`TEAMVIEWRELAY_SOURCE_REFERER`、`TEAMVIEWRELAY_DISPLAY_NAME`、`TEAMVIEWRELAY_POLL_INTERVAL_SECS`、`TEAMVIEWRELAY_FAILURE_GRACE_SECS`、`TEAMVIEWRELAY_NORMALIZE_DIMENSIONS`、`TEAMVIEWRELAY_SOURCE_ID`、`TEAMVIEWRELAY_HISTORY_STATE_PATH`、`TEAMVIEWRELAY_HISTORY_RETENTION_DAYS` 和 `TEAMVIEWRELAY_HISTORY_FLUSH_INTERVAL_SECS`。

### EdgeOne 真人验证会话

如果 EdgeOne 对 `players.json` 显示真人验证，可以在本机浏览器完成验证后，在开发者工具中对成功请求使用 `Copy as cURL`。只复制 `-b '...'` 参数中的 Cookie 值，保存为独立文件，例如 `data/map1.cookies`，然后在配置中启用：

```toml
source_url = "https://map1.nodemc.cc/tiles/players.json?"
source_cookie_file = "data/map1.cookies"
source_user_agent = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36 Edg/150.0.0.0"
source_referer = "https://map1.nodemc.cc/tiles/players.json?"
```

Cookie 文件是敏感凭证，请限制文件权限并不要提交到 Git。程序会在每次轮询时重新读取该文件，因此真人验证过期后，重新导出 Cookie 并替换文件即可，无需重启客户端。复制出来的 `if-none-match`、`sec-*`、`priority` 和 `upgrade-insecure-requests` 不需要配置：客户端会自动维护 ETag，并只发送必要的请求头。

验证页失效时，客户端会向 Relay/Backend 上报明确的 `captcha_required` 原因，并继续遵守 `failure_grace_secs` 的降级和清空策略；不会把它笼统归类为 `json`。

历史状态默认写入 `data/history-v1.json`，默认保留已下线玩家 90 天。将 `history_retention_days` 设为 `0` 可永久保留。程序使用内存索引和节流的原子 JSON 快照，不引入数据库；下线、重新上线和过期删除立即落盘，持续在线确认最多每 60 秒合并落盘。HTTP 客户端只启用 Squaremap 所需的 HTTP/1.1，单线程 Tokio runtime、musl 静态链接和 scratch 运行镜像共同控制常驻资源。

`normalize_dimensions` 默认开启，把 Squaremap 扁平化世界键的第一个 `_` 还原成 Minecraft 资源标识符的 `:`，例如 `minecraft_overworld` 转换为 `minecraft:overworld`、`minecraft_the_nether` 转换为 `minecraft:the_nether`。已经包含 `:` 或没有 `_` 的值保持不变；设为 `false` 可完全保留上游 `world`。

## Docker

运行镜像使用静态 MUSL 二进制和 `scratch`，不包含 shell、包管理器或动态运行库。构建时只额外传入协议的 `proto` 目录：

```bash
docker build \
  --build-context protocol=../TeamViewRelay-Protocol/proto \
  -t professornuo/team-view-relay-squaremap-source-client:v0.2.0-proto0.6.4 .
```

`compose.example.yml` 提供了低占用运行配置：无端口暴露、只读文件系统、无 Linux capabilities、`0.25 CPU`、`64MB` 内存和最多 32 个进程。准备配置后运行：

```bash
cp config.example.toml config.toml
docker compose -f compose.example.yml up -d --build
```

Compose 示例默认以 UID `0` 运行，是为了兼容宿主机上常见的 root-owned/`0600` `config.toml` bind mount。由于最终运行镜像是 `scratch`，不应在 Compose 中写 `0:0`，否则 Docker 可能尝试从不存在的 `/etc/group` 解析组 `0`；镜像自身仍默认使用 UID/GID `65532:65532`。如果你希望 Compose 也使用非 root 用户，请先确保挂载的配置、Cookie 文件和数据目录都能被该 UID 读取/写入，再设置：

```bash
TEAMVIEWRELAY_CONTAINER_UID="$(id -u)" \
  docker compose -f compose.example.yml up -d --build
```

如果启用了真人验证 Cookie，使用绝对容器路径并把文件只读挂载进去，例如在 Compose 的 `volumes` 中增加 `./data/map1.cookies:/data/map1.cookies:ro`，配置写成 `source_cookie_file = "/data/map1.cookies"`。

如果 Docker Hub 不可达，可通过环境变量覆盖构建镜像，例如：

```bash
RUST_IMAGE=docker.1ms.run/library/rust:1.94.1-bookworm \
  docker compose -f compose.example.yml build
```

离线打包时，可先在相同 Linux 架构的主机上构建静态 MUSL 二进制，再只将该二进制封装进镜像；这样无需拉取 Rust 构建镜像，运行层仍为 `scratch`：

```bash
cargo build --locked --release --target x86_64-unknown-linux-musl
docker build \
  --build-context binary=target/x86_64-unknown-linux-musl/release \
  -f Dockerfile.prebuilt \
  -t professornuo/team-view-relay-squaremap-source-client:v0.2.0-proto0.6.4 .
```

上游请求失败时，客户端不会刷新旧玩家对象。连续失败达到 30 秒后会清空自己上报的玩家和 Tab 状态；`200` 和 `304` 都视为成功确认。

### 可选 pass-cdn 浏览器 sidecar

Rust 客户端仍支持直接读取源站；如果 EdgeOne 验证频繁失效，可以让
`pass-cdn` 保持一个 Chromium 会话，再把 Rust 的 `source_url` 指向 sidecar：

```toml
source_url = "http://pass-cdn:8080/tiles/players.json"
source_id = "00000000-0000-0000-0000-000000000001"
```

sidecar 提供与源站相同的 JSON 接口，并实现 `ETag`/`304`、健康检查和过期
`503`。Rust 不需要启动 Python 或 Chrome，仍使用原有的 HTTP 轮询和失败宽限。

仓库提供独立的 `compose.pass-cdn.example.yml`，不会改变现有直连 Compose：

```bash
cp config.pass-cdn.example.toml config.pass-cdn.toml
docker compose -f compose.pass-cdn.example.yml up -d --build
```

切换到 sidecar 时请固定 `source_id`。历史文件现在只按 `source_id` 和房间
校验，传输地址从源站切换为 sidecar 不会丢失离线历史。

## 数据与故障语义

- Squaremap 返回的 32 位 UUID 会规范为小写带横线 UUID。
- 具有有效 UUID 和名称的条目进入 Tab；只有 `world` 非空且 `x/y/z` 都是有限数值时才上报位置。
- `world` 默认按 `normalize_dimensions` 规则转换后作为 `dimension`（关闭该选项时原样保留），
  同时映射有限的 `health` 和 `armor`；忽略 `yaw` 和 `max`。
- 健康状态按 `STARTING -> HEALTHY -> DEGRADED -> UNAVAILABLE` 变化。降级期间不会发送玩家或 Tab keepalive，恢复时重新发布完整状态。
- 有效空名单会立即清空状态；错误根对象、非数组 `players` 或非空但完全无法解析的名单会进入失败状态。
- 有 UUID 和名称但没有坐标的玩家仍视为在线；只有曾获得有效位置的玩家才会生成可渲染的离线历史。
- `200` 响应更新位置采样时间；`304` 只刷新当前名单的最后在线确认时间。上游失败不会制造下线事件。

0.6.4 Relay 会接收并转发离线历史。旧 Relay 不会收到历史字段，实时上报仍使用兼容 patch，避免旧版对零坐标 replace 的解码问题。

## 发布

推送 `v*` tag 后，GitHub Actions 会构建 Linux x86_64、Linux aarch64 和 Windows x86_64 产物。仓库根目录的 `Dockerfile` 生成静态、非 root 的最小运行镜像。
