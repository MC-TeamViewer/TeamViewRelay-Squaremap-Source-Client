# TeamViewRelay Squaremap Source Client

这个项目读取 Squaremap 的在线玩家数据，并把玩家位置发送到 TeamViewRelay。

当前要读取的地址是：

```text
https://map1.nodemc.cc/tiles/players.json
```

这个地址可能出现 EdgeOne 真人验证，所以项目提供两种启动方式：

1. 浏览器模式：自动启动无界面的 Chromium 完成验证，适合云服务器，推荐使用。
2. 直连模式：直接请求源站，不启动 Chromium；只有源站允许普通 HTTP 请求时才能使用。

## 准备目录

构建时需要 TeamViewRelay 的协议文件。两个仓库应放在同一目录：

```text
MC-mods/
├── TeamViewRelay-Protocol/
│   └── proto/
└── TeamViewRelay-Squaremap-Source-Client/
```

服务器只需安装 Docker 和 Docker Compose，不需要安装 Python，也不需要在宿主机安装 Chromium。

## 推荐：浏览器模式

这是读取 `map1.nodemc.cc` 的推荐方式。

### 1. 创建配置

```bash
cp config.pass-cdn.example.toml config.toml
```

打开 `config.toml`，至少修改下面两项：

```toml
# TeamViewRelay 的玩家客户端地址，必须以 /mc-client 结尾。
# 这个地址必须能从 Docker 容器内访问，不能继续使用示例中的 127.0.0.1。
relay_url = "wss://你的-TeamViewRelay-地址/mc-client"

# 玩家数据要发送到的 TeamViewRelay 房间。
room_code = "你的房间代码"
```

`source_url` 不要改，它已经指向负责读取源站的容器：

```toml
source_url = "http://pass-cdn:8080/tiles/players.json"
```

其他配置可以先保持默认。

### 2. 构建并启动

```bash
docker compose --profile browser up -d --build
```

第一次构建会下载 Rust 和 Chromium，所需时间取决于网络速度。

这条命令会启动两个容器：`pass-cdn` 负责从 `map1.nodemc.cc` 取数，`squaremap-source` 负责把取到的玩家数据发送给 TeamViewRelay。

### 3. 确认是否成功

```bash
docker compose --profile browser ps
```

`pass-cdn` 显示 `healthy` 后，再查看日志：

```bash
docker compose --profile browser logs -f pass-cdn squaremap-source
```

成功时应看到：

- `pass-cdn` 周期性输出 `players.json updated`
- `squaremap-source` 成功连接 TeamViewRelay
- TeamViewRelay 对应房间中出现 Squaremap 玩家

按 `Ctrl+C` 只会退出日志查看，不会停止容器。

### 4. 停止

```bash
docker compose --profile browser down
```

## 直连模式

如果 `players.json` 可以直接返回 JSON，没有 EdgeOne 验证，可以不用 Chromium。

### 1. 创建配置

```bash
cp config.example.toml config.toml
```

打开 `config.toml`，修改：

```toml
relay_url = "wss://你的-TeamViewRelay-地址/mc-client"
room_code = "你的房间代码"
source_url = "https://map1.nodemc.cc/tiles/players.json"
```

### 2. 构建并启动

```bash
docker compose up -d --build
```

### 3. 查看日志

```bash
docker compose logs -f squaremap-source
```

如果日志提示 `captcha_required`，说明源站要求真人验证，请停止直连模式并改用上面的浏览器模式：

```bash
docker compose down
cp config.pass-cdn.example.toml config.toml
```

然后重新填写 `relay_url` 和 `room_code`，再执行：

```bash
docker compose --profile browser up -d --build
```

## 构建镜像失败

如果服务器无法从 Docker Hub 下载基础镜像，可以使用镜像站重新构建：

```bash
RUST_IMAGE=docker.1ms.run/library/rust:1.94.1-bookworm \
DEBIAN_IMAGE=docker.1ms.run/library/debian:bookworm-slim \
  docker compose --profile browser up -d --build
```

## 常用命令

浏览器模式：

```bash
docker compose --profile browser ps
docker compose --profile browser logs -f
docker compose --profile browser restart
docker compose --profile browser down
```

直连模式：

```bash
docker compose ps
docker compose logs -f
docker compose restart
docker compose down
```

玩家离线历史保存在 Docker volume `squaremap-history` 中。普通的 `docker compose down` 不会删除它；不要使用 `docker compose down -v`，除非确定要删除历史数据。
