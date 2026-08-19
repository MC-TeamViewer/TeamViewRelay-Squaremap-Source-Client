# pass-cdn

这个目录提供一个使用 Chromium 浏览器读取 EdgeOne 保护的 Squaremap
`players.json` 的辅助服务。它不会替代 Rust source client；Rust 仍然可以
直接访问源站，pass-cdn 只是可选的 HTTP sidecar。

## 一次性取数

在本目录中运行：

```bash
uv run python main.py
```

默认使用有头浏览器，完成验证后把 JSON 打印到 stdout。需要无头模式时：

```bash
uv run python main.py --headless
```

## HTTP sidecar

长期运行时使用：

```bash
uv run python main.py --serve --headless --host 127.0.0.1 --port 8080
```

接口为：

- `GET /tiles/players.json`：返回最新 JSON，支持 `ETag` 和 `304`；
- `GET /healthz`：没有新鲜数据时返回 `503`。

默认缓存超过 30 秒就视为过期。此时 Rust 客户端会按现有的失败宽限策略
处理，不会继续使用无限期旧数据。

容器部署可直接使用仓库根目录的 `compose.pass-cdn.example.yml`。复制
`config.pass-cdn.example.toml` 为 `config.pass-cdn.toml` 后启动：

```bash
docker compose -f compose.pass-cdn.example.yml up -d --build
```

Compose 中 Chromium 使用无头模式；本机排障或首次验证可以省略 `--headless`。
pass-cdn 默认不向宿主机发布端口，只允许同一 Compose 网络中的 Rust 客户端访问。

## 配置注意事项

- `--url` 可以替换为其他 Squaremap `players.json` 地址；
- `--poll-interval` 控制浏览器刷新周期；
- `--max-stale-secs` 控制 sidecar 允许提供缓存的最长时间；
- Chrome 路径默认优先使用系统中的 Chromium/Chrome，也兼容 `main.py` 旁的本地浏览器目录；
- 验证令牌只保留在浏览器会话中，不写入 Rust Cookie 文件，也不会打印到日志。
