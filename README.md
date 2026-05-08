# termd

**termd** 是一个面向个人开发者的端到端加密持久终端系统。当前 MVP 重点是：daemon 持有 PTY session，受信任设备可以重新 attach，同一个 session 不会因为某个 client 断开而终止。

项目定位是个人使用：单个 daemon、设备级信任、一个当前 controller，加上其他 viewer 设备。当前仓库仍处于 MVP 阶段，下面的“已验证”只表示当前代码和本地 QA 覆盖的能力；完整 Native client 还没有交付。

## 安装方式

release 资产和 GHCR 镜像由 tag 驱动。下面是一键安装入口；如果你要固定版本，把 `latest` 换成对应 tag。

### termctl

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

### termd

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web
```

如果需要让局域网或公网反向代理访问 daemon，可以直接在安装时设置监听地址：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --listen 0.0.0.0:8765
```

安装脚本会注册并启动 `termd.service`，随后在当前终端打印一个短期一次性 pairing token 和 `termctl pair` 示例。token 不会写入配置文件；过期或用过后可在 daemon 主机上运行 `termd pair` 重新签发。

### termrelay

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --listen 0.0.0.0:8080
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --listen 0.0.0.0:8080
```

`termrelay` 也提供 [docker-compose 部署方式](docs/deployment.md#termrelay-docker-compose)。

`termd` 和 `termrelay` 的 systemd 安装脚本都可以通过 `bash -s -- ...` 追加安装参数。常用参数包括 `--web`、`--no-web`、`--listen <HOST:PORT>`、`--public`；也可以在对应的 `/etc/termd/*.env` 里设置 `TERMD_WEB_ENABLED=1`、`TERMRELAY_WEB_ENABLED=1` 或监听地址，脚本会自动组装启动参数。

---

## 当前状态

| 能力 | 当前状态 | 说明 |
| --- | --- | --- |
| PTY/session runtime | 已验证 | `termd` 可以创建 PTY session，session 生命周期独立于单个 client 连接。 |
| 多客户端 attach | 已验证 | 第一个 attach 获得 controller，其他设备是 viewer。 |
| 控制权抢占 | 已验证 | 已配对设备可以发送 `control_request` 抢占唯一 controller。 |
| direct WebSocket daemon | 已验证 | `termd` 默认监听 `127.0.0.1:8765`，WebSocket 路径是 `/ws`，健康检查是 `/healthz`。 |
| 设备级 pairing/auth | 已验证 | `termd pair` 可向运行中的本机 daemon 签发短期 pairing token；pairing token 过期、device key 验证、challenge-response 和 timestamp/nonce replay protection 已实现。 |
| E2EE frame 边界 | 已验证 | 当前实现是 X25519 + HKDF + ChaCha20Poly1305 `encrypted_frame`，不是 Noise protocol。 |
| `termctl` CLI | 已验证 | 当前子命令是 `pair/new/attach/control/resize/list`；QA 覆盖真实 `termd pair` 到 `termctl pair --token` 闭环、direct daemon E2E 和 relay runtime E2E。 |
| `termrelay` dumb pipe | 已验证 | relay 按公开 `server_id` 路由 WebSocket frame；mux 路径只包装 `client_id` 和不透明 frame，不解密、不解析业务 envelope、不判断 controller/viewer 控制权。 |
| `termui/frontend` Web MVP | 已验证 | 支持 pairing token consumer、session list、terminal attach、controller/viewer 状态、control request 和 IndexedDB 状态边界；QA 覆盖浏览器通过真实 relay client URL 完成 pairing/list。 |
| `termui/native` Flutter 骨架 | 架构骨架 | 只有 Native app/service/storage/protocol 分层和安全边界测试；还不是完整 Native client。 |
| SSH 会话复用 | 目标场景 | 当前没有专门 SSH 管理层；可以把 `ssh` 当作普通 PTY 命令运行。 |
| daemon 主动连接 relay | 已验证 | `termd --relay ws://host:port` 会连接 relay 的 daemon mux 路径；可重复传入多个 `--relay` / `--relay-url` 端点。Web/termctl 可使用 `ws://relay/ws/{server_id}/client`；公网部署方案见 [docs/deployment.md](docs/deployment.md)。 |
| 扫码 / 二维码 pairing | 已验证 | `termd pair --qr` 可输出二维码 payload，并支持 payload 消费。 |
| 安装脚本 / GHCR 发布 | 已提供 | `scripts/install-termctl.sh`、`scripts/install-termd.sh`、`scripts/install-termrelay.sh` 支持 curl/wget 安装；`termd` 安装后会打印一个短期一次性 pairing token；Linux amd64 release tarball 使用 musl 静态链接二进制；`termd` 和 `termrelay` 的 systemd 安装脚本支持通过 `bash -s -- --web --listen ...` 写入配置；tag 触发的 GitHub Actions 会同时发布 release 资产和 GHCR 镜像，`termrelay` 另有 docker-compose 方案。 |

非目标：把 termd 做成多人平台。当前设计只面向个人使用和设备级信任。

---

## 快速开始

### 1. 运行统一 QA

```bash
bash scripts/qa.sh
```

脚本会从任意目录切回仓库根目录，依次运行 Rust、direct E2E、relay E2E、relay runtime E2E、termui Web 和 termui Native 可用验证。当前环境没有 Flutter/Dart 时，脚本会明确记录未运行 `flutter analyze/test/build`，并执行 Native 结构与敏感字符串 fallback 检查。

更短的 Rust-only 验证：

```bash
cargo fmt --all -- --check
cargo test --workspace
```

### 2. 启动 daemon

```bash
cargo run -p termd
```

默认监听：

```text
HTTP health: http://127.0.0.1:8765/healthz
WebSocket:   ws://127.0.0.1:8765/ws
```

健康检查：

```bash
curl http://127.0.0.1:8765/healthz
```

### 3. 生成 pairing token 并配对设备

在另一个 shell 中向运行中的 daemon 签发一次性 token：

```bash
PAIRING_TOKEN="$(cargo run -q -p termd -- pair)"
```

`termd pair` 默认请求 `http://127.0.0.1:8765/local/pairing-token`，stdout 只输出 token。需要覆盖 daemon HTTP 地址时：

```bash
cargo run -q -p termd -- pair --url http://127.0.0.1:8765
```

使用 token 配对当前 `termctl` 设备：

`--state` 是全局参数，可用于指定本地设备状态文件；不传时默认使用 `TERMD_CTL_STATE` 或 `$HOME/.termd/termctl-state.json`。

```bash
cargo run -p termctl -- pair --token "$PAIRING_TOKEN" --url ws://127.0.0.1:8765/ws
```

### 4. 当前 `termctl` session 命令形态

```bash
cargo run -p termctl -- new --url ws://127.0.0.1:8765/ws -- /bin/sh
```

```bash
cargo run -p termctl -- list --url ws://127.0.0.1:8765/ws
```

```bash
cargo run -p termctl -- attach <session_id> --url ws://127.0.0.1:8765/ws
```

```bash
cargo run -p termctl -- control <session_id> --url ws://127.0.0.1:8765/ws
```

```bash
cargo run -p termctl -- resize <session_id> --rows 40 --cols 120 --url ws://127.0.0.1:8765/ws
```

把 SSH 放进持久 PTY 时，按普通命令启动：

```bash
cargo run -p termctl -- new --url ws://127.0.0.1:8765/ws -- /bin/sh -lc "ssh user@host"
```

### 5. 运行 Web MVP

```bash
cd termui/frontend
npm ci
npm run typecheck
npm run test -- --run
npm run build
npm run test:e2e
npm audit --audit-level=high
```

Web MVP 是 pairing token consumer：它能使用已有 token 完成配对、认证、session list、attach 和 control request，但不提供 daemon 侧 token 签发入口。

### 6. 运行 Native 骨架验证

如果本机有 Flutter/Dart SDK：

```bash
cd termui/native
flutter pub get
flutter analyze
flutter test
```

如果没有 Flutter/Dart SDK，不能把 Native 写成已完成验证的 client。`bash scripts/qa.sh` 会记录未运行 `flutter analyze/test/build`，并执行结构检查、敏感字符串检查和 UI 分层边界检查。

### 7. 启动 relay

```bash
cargo run -p termrelay -- --listen 127.0.0.1:8080
```

relay 路径：

```text
ws://127.0.0.1:8080/ws/{server_id}/daemon
ws://127.0.0.1:8080/ws/{server_id}/daemon-mux
ws://127.0.0.1:8080/ws/{server_id}/client
```

relay 只转发 WebSocket frame。它不会替 daemon 做 pairing/auth/session/control，也不会解析 E2EE 业务 envelope。

启动 daemon 并主动注册到 relay：

```bash
cargo run -p termd -- --relay ws://127.0.0.1:8080
```

另一个 shell 中生成 token 并读取 `server_id`：

```bash
PAIRING_TOKEN="$(cargo run -q -p termd -- pair)"
SERVER_ID="$(curl -s http://127.0.0.1:8765/healthz | python3 -c 'import json,sys; print(json.load(sys.stdin)["server_id"])')"
```

`termctl` 或 Web MVP 使用 relay client URL：

```bash
cargo run -p termctl -- pair --token "$PAIRING_TOKEN" --url "ws://127.0.0.1:8080/ws/${SERVER_ID}/client"
cargo run -p termctl -- list --url "ws://127.0.0.1:8080/ws/${SERVER_ID}/client"
```

当前 relay 支持本地 MVP 和多 relay endpoint；公网部署、反向代理、health check 与日志边界见 [docs/deployment.md](docs/deployment.md)。

---

## QA 与生成物

最终 QA 矩阵见 [docs/qa.md](docs/qa.md)。

生成物不是交付源码：`termui/frontend/node_modules/`、`termui/frontend/dist/`、`termui/frontend/test-results/`、`termui/frontend/playwright-report/`、`termui/native/.dart_tool/`、`termui/native/build/` 和 `termui/native/coverage/` 都只作为本地验证副产物处理。

---

## 安全模型

```text
daemon public key = trust anchor
device key = identity
pairing = trust establishment
relay = untrusted dumb pipe
```

当前已实现并回归的边界：

* 未配对设备不能完成认证。
* pairing token 有过期时间和一次性消费边界。
* auth 使用 challenge-response，并带 timestamp/nonce replay protection。
* session 作用域操作必须由当前连接先 attach 到目标 session。
* 终端业务消息进入 E2EE `encrypted_frame` 后再传输。
* relay 只看到公开 `server_id`、frame 元数据和密文，不接触明文业务。
* Web IndexedDB 不保存 pairing token、server private key 或 terminal transcript。
* Native 骨架在 secure storage 不可用时 fail closed，不降级到明文文件或 SharedPreferences。

---

## 通信协议

传输层统一使用 HTTP + WebSocket：

```text
termd direct: ws://127.0.0.1:8765/ws
termrelay:    ws://<relay>/ws/{server_id}/daemon
termrelay:    ws://<relay>/ws/{server_id}/daemon-mux
termrelay:    ws://<relay>/ws/{server_id}/client
```

外层消息格式：

```json
{
  "type": "string",
  "payload": {}
}
```

敏感业务消息必须在 E2EE 内层 envelope 中传输。`session_data` 当前使用 base64 承载终端字节。

---

## 当前仓库结构

```text
/proto               # 共享协议类型和 wire envelope
/termd               # daemon：PTY、session runtime、auth、E2EE、HTTP/WebSocket
/termctl             # CLI：pair/new/attach/control/resize/list
/termrelay           # relay：按 server_id 路由 WebSocket frame
/termui/frontend     # Web MVP
/termui/native       # Flutter Native 架构骨架
/scripts             # 本地 QA 脚本
/docs                # 短交付文档
```

---

## License

MIT. See [LICENSE](LICENSE).
