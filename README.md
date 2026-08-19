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

也可以使用环境变量覆盖配置：`TEAMVIEWRELAY_RELAY_URL`、`TEAMVIEWRELAY_ROOM_CODE`、`TEAMVIEWRELAY_SOURCE_URL`、`TEAMVIEWRELAY_DISPLAY_NAME`、`TEAMVIEWRELAY_POLL_INTERVAL_SECS`、`TEAMVIEWRELAY_FAILURE_GRACE_SECS`、`TEAMVIEWRELAY_NORMALIZE_DIMENSIONS`、`TEAMVIEWRELAY_SOURCE_ID`、`TEAMVIEWRELAY_HISTORY_STATE_PATH`、`TEAMVIEWRELAY_HISTORY_RETENTION_DAYS` 和 `TEAMVIEWRELAY_HISTORY_FLUSH_INTERVAL_SECS`。

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
