# -*- coding: utf-8 -*-
"""通过持久化 Chromium 会话读取 EdgeOne 保护的 Squaremap players.json。

默认行为保留一次性模式：完成验证后把 JSON 打印到 stdout。
使用 ``--serve`` 时，程序会保持浏览器会话并提供一个本地 HTTP sidecar：

    GET /tiles/players.json
    GET /healthz

sidecar 会提供 ETag/304，并在缓存超过最大新鲜度后返回 503，供 Rust
source client 继续使用现有的 HTTP 轮询、失败宽限和清空逻辑。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import threading
import time
from dataclasses import dataclass
from email.utils import formatdate
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from random import uniform
from typing import Any
from urllib.parse import urlsplit

import requests
from DrissionPage import ChromiumOptions, ChromiumPage
from DrissionPage.errors import ElementLostError, ElementNotFoundError

DEFAULT_URL = "https://map1.nodemc.cc/tiles/players.json"
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 8080
DEFAULT_POLL_INTERVAL = 5.0
DEFAULT_MAX_STALE = 30.0
UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36"
)

# 验证过程中页面会多次刷新，元素对象随时失效，这两类异常按瞬态处理。
TRANSIENT = (ElementNotFoundError, ElementLostError)


def retry(fn, timeout: float, interval: float = 0.5):
    """反复执行 fn 直到返回真值或超时。"""
    deadline = time.time() + timeout
    while True:
        try:
            result = fn()
            if result:
                return result
        except TRANSIENT:
            pass
        if time.time() > deadline:
            return None
        time.sleep(interval)


def cookies_dict(page: ChromiumPage) -> dict[str, str]:
    try:
        return {cookie["name"]: cookie["value"] for cookie in page.cookies()}
    except Exception:
        return {}


def human_path(sx, sy, tx, ty, steps=16):
    """生成从起点到目标的拟人鼠标轨迹。"""
    dx, dy = tx - sx, ty - sy
    length = (dx * dx + dy * dy) ** 0.5 or 1.0
    points = []
    for i in range(steps + 1):
        t = i / steps
        easing = t * t * (3 - 2 * t)
        wobble = 6 * (1 - abs(2 * t - 1)) * uniform(-1, 1)
        x = sx + dx * easing - dy / length * wobble
        y = sy + dy * easing + dx / length * wobble
        points.append((x, y))
    return points


def mouse_click(page, frame, target):
    """在页面坐标系中模拟真人鼠标移动和点击。"""
    ix, iy = frame.rect.location
    cx, cy = target.rect.click_point
    ax, ay = ix + cx, iy + cy
    sx = max(20.0, min(ax + uniform(-150, 150), 1400.0))
    sy = max(20.0, min(ay + uniform(-120, 120), 800.0))
    points = human_path(sx, sy, ax, ay)
    page.actions.move_to(points[0], duration=0.02)
    for (x1, y1), (x2, y2) in zip(points, points[1:]):
        page.actions.move(x2 - x1, y2 - y1, duration=uniform(0.02, 0.03))
    page.actions.wait(uniform(0.02, 0.08)).hold() \
        .wait(uniform(0.03, 0.08)).release()


def wait_cookie(page: ChromiumPage, name: str, timeout: float):
    deadline = time.time() + timeout
    while time.time() < deadline:
        cookies = cookies_dict(page)
        if name in cookies:
            return cookies
        time.sleep(0.2)
    return cookies_dict(page)


def read_json_from_page(page: ChromiumPage) -> dict[str, Any] | None:
    """读取浏览器当前文档中的 JSON，不脱离浏览器上下文。"""
    for selector in ("tag:pre", "tag:body"):
        try:
            element = page.ele(selector, timeout=1)
            if not element:
                continue
            text = element.text.strip()
            if not text.startswith("{"):
                continue
            value = json.loads(text)
            if isinstance(value, dict):
                return value
        except (json.JSONDecodeError, ElementNotFoundError, ElementLostError):
            continue
    return None


class BrowserFetcher:
    """持有一个 Chromium 会话，并负责验证、刷新和读取 JSON。"""

    def __init__(self, url: str, headed: bool, chrome_path: Path | None = None):
        self.url = url
        self.headed = headed
        self.chrome_path = chrome_path
        self.page: ChromiumPage | None = None

    def _find_chrome(self) -> Path | None:
        if self.chrome_path is not None:
            return self.chrome_path
        candidates = (
            Path("/usr/bin/chromium"),
            Path("/usr/bin/chromium-browser"),
            Path("/usr/bin/google-chrome"),
            Path(__file__).resolve().parent / "chrome-linux" / "chrome",
        )
        return next((path for path in candidates if path.is_file()), None)

    def start(self) -> None:
        options = ChromiumOptions()
        options.headless(not self.headed)
        options.auto_port()
        chrome_path = self._find_chrome()
        if chrome_path is not None:
            options.set_browser_path(str(chrome_path))
        options.set_argument("--disable-blink-features=AutomationControlled")
        options.set_argument("--window-size=1600,900")
        try:
            options.set_user_agent(UA)
        except AttributeError:
            pass
        self.page = ChromiumPage(options)

    def close(self) -> None:
        if self.page is None:
            return
        try:
            self.page.quit()
        except Exception:
            pass
        self.page = None

    def _solve_challenge(self) -> None:
        if self.page is None:
            raise RuntimeError("browser is not started")

        wait_cookie(self.page, "EO_Bot_Ssid", 5)
        frame = retry(
            lambda: self.page.get_frame("tcaptcha_iframe_eo", timeout=3),
            40,
        )
        if frame is None:
            raise RuntimeError("未找到验证 iframe")

        checkbox = retry(
            lambda: frame.ele("css:#verifyCheckbox", timeout=3),
            15,
        )
        if checkbox is None:
            raise RuntimeError("未找到复选框 #verifyCheckbox")

        mouse_click(self.page, frame, checkbox)
        cookies = wait_cookie(self.page, "EO-Bot-Captcha-Token", 6)
        if "EO-Bot-Captcha-Token" not in cookies:
            raise RuntimeError("验证未通过，未拿到 EO-Bot-Captcha-Token")

    def _wait_json(self, timeout: float) -> dict[str, Any] | None:
        if self.page is None:
            return None
        return retry(lambda: read_json_from_page(self.page), timeout, 0.2)

    def fetch(self) -> dict[str, Any]:
        if self.page is None:
            raise RuntimeError("browser is not started")

        # 每次都从稳定的源站地址开始，让浏览器根据当前会话重新完成必要的
        # 重定向；不要长期复用上一次带验证码查询参数的 URL。
        self.page.get(self.url)
        data = self._wait_json(5)
        if data is None:
            self._solve_challenge()
            data = self._wait_json(5)

        if data is None:
            # 兜底仍使用浏览器当前 URL 和 Cookie；验证码令牌可能只存在于
            # 重定向 URL 中，因此不能只把 Cookie 交给 Rust 直连。
            cookies = cookies_dict(self.page)
            response = requests.get(
                self.page.url,
                cookies=cookies,
                headers={"User-Agent": UA},
                timeout=20,
            )
            response.raise_for_status()
            content_type = response.headers.get("Content-Type", "")
            if "json" not in content_type.lower():
                raise RuntimeError(
                    f"未获取到 JSON，响应 Content-Type 为 {content_type or 'unknown'}"
                )
            value = response.json()
            if not isinstance(value, dict):
                raise RuntimeError("players JSON 根对象不是 object")
            data = value

        return data


@dataclass(frozen=True)
class CachedSnapshot:
    body: bytes
    etag: str
    updated_at: float
    last_error: str | None


class SnapshotCache:
    def __init__(self):
        self._lock = threading.Lock()
        self._snapshot: CachedSnapshot | None = None
        self._last_error: str | None = None

    def update(self, value: dict[str, Any]) -> CachedSnapshot:
        body = json.dumps(
            value,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
        etag = '"sha256-' + hashlib.sha256(body).hexdigest() + '"'
        snapshot = CachedSnapshot(body, etag, time.time(), None)
        with self._lock:
            self._snapshot = snapshot
            self._last_error = None
        return snapshot

    def fail(self, error: Exception) -> None:
        with self._lock:
            self._last_error = str(error)

    def get(self, max_stale: float) -> tuple[CachedSnapshot | None, float, str | None]:
        with self._lock:
            snapshot = self._snapshot
            error = self._last_error
        if snapshot is None:
            return None, float("inf"), error
        age = max(0.0, time.time() - snapshot.updated_at)
        return (snapshot if age <= max_stale else None), age, error


class SidecarHandler(BaseHTTPRequestHandler):
    server: "SidecarServer"

    def _json_response(
        self,
        status: int,
        payload: bytes,
        headers: dict[str, str] | None = None,
    ) -> None:
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(payload)

    def do_GET(self):  # noqa: N802 - BaseHTTPRequestHandler API
        self._handle()

    def do_HEAD(self):  # noqa: N802 - BaseHTTPRequestHandler API
        self._handle()

    def _handle(self) -> None:
        path = urlsplit(self.path).path
        snapshot, age, error = self.server.cache.get(self.server.max_stale)

        if path == "/healthz":
            payload = json.dumps(
                {
                    "ok": snapshot is not None,
                    "age_secs": None if age == float("inf") else round(age, 3),
                    "last_error": error,
                },
                ensure_ascii=False,
                separators=(",", ":"),
            ).encode("utf-8")
            self._json_response(200 if snapshot is not None else 503, payload)
            return

        if path != "/tiles/players.json":
            self._json_response(
                HTTPStatus.NOT_FOUND,
                b'{"error":"not_found"}',
            )
            return

        if snapshot is None:
            payload = json.dumps(
                {
                    "error": "snapshot_unavailable",
                    "age_secs": None if age == float("inf") else round(age, 3),
                    "detail": error,
                },
                ensure_ascii=False,
                separators=(",", ":"),
            ).encode("utf-8")
            self._json_response(HTTPStatus.SERVICE_UNAVAILABLE, payload)
            return

        if self.headers.get("If-None-Match") == snapshot.etag:
            self.send_response(HTTPStatus.NOT_MODIFIED)
            self.send_header("ETag", snapshot.etag)
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            return

        self._json_response(
            HTTPStatus.OK,
            snapshot.body,
            {
                "ETag": snapshot.etag,
                "Last-Modified": formatdate(snapshot.updated_at, usegmt=True),
                "X-Snapshot-Age": f"{age:.3f}",
            },
        )


class SidecarServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, cache: SnapshotCache, max_stale: float):
        super().__init__(address, SidecarHandler)
        self.cache = cache
        self.max_stale = max_stale


def run_sidecar(args: argparse.Namespace) -> int:
    cache = SnapshotCache()
    fetcher = BrowserFetcher(
        args.url,
        headed=not args.headless,
        chrome_path=args.chrome_path,
    )
    stop = threading.Event()

    def worker() -> None:
        try:
            fetcher.start()
            while not stop.is_set():
                try:
                    cache.update(fetcher.fetch())
                    print("[*] players.json 已更新", file=sys.stderr, flush=True)
                except Exception as error:
                    cache.fail(error)
                    print(f"[!] 取数失败: {error}", file=sys.stderr, flush=True)
                stop.wait(args.poll_interval)
        finally:
            fetcher.close()

    worker_thread = threading.Thread(target=worker, name="browser-fetcher", daemon=True)
    worker_thread.start()
    server = SidecarServer((args.host, args.port), cache, args.max_stale_secs)
    print(
        f"[*] sidecar listening on http://{args.host}:{args.port}/tiles/players.json",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        server.shutdown()
        server.server_close()
        worker_thread.join(timeout=max(5.0, args.poll_interval + 2.0))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serve", action="store_true", help="运行 HTTP sidecar")
    parser.add_argument("--url", default=DEFAULT_URL, help="目标 players.json URL")
    parser.add_argument("--host", default=DEFAULT_HOST, help="sidecar 监听地址")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--poll-interval", type=float, default=DEFAULT_POLL_INTERVAL)
    parser.add_argument("--max-stale-secs", type=float, default=DEFAULT_MAX_STALE)
    parser.add_argument("--headless", action="store_true", help="使用无头 Chromium")
    parser.add_argument("--chrome-path", type=Path, default=None)
    return parser


def run_once(args: argparse.Namespace) -> int:
    fetcher = BrowserFetcher(
        args.url,
        headed=not args.headless,
        chrome_path=args.chrome_path,
    )
    try:
        fetcher.start()
        data = fetcher.fetch()
        print(json.dumps(data, ensure_ascii=False, indent=2))
        return 0
    except Exception as error:
        print(f"[!] {error}", file=sys.stderr)
        return 1
    finally:
        fetcher.close()


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")
    except (AttributeError, ValueError):
        pass
    args = build_parser().parse_args()
    if args.poll_interval <= 0 or args.max_stale_secs <= 0:
        raise SystemExit("--poll-interval 和 --max-stale-secs 必须为正数")
    if not 1 <= args.port <= 65535:
        raise SystemExit("--port 必须在 1 到 65535 之间")
    return run_sidecar(args) if args.serve else run_once(args)


if __name__ == "__main__":
    sys.exit(main())
