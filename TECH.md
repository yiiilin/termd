# 技术架构

本文记录当前代码库的真实技术状态。当前 Rust workspace 包含 `proto`、`termd`、`termctl` 和 `termrelay`；`termui/frontend` 是已验证的 Web MVP，`termui/native` 是 Flutter Native 架构骨架。

---

## 当前 Rust Workspace

```text
/proto
/termd
/termctl
/termrelay
```

前端与 Native 目录不在 Cargo workspace 内：

```text
/termui/frontend
/termui/native
```

### `proto`

`proto` 定义跨组件共享的协议类型，包括：

* message envelope 和 message type。
* `server_id`、`device_id`、`session_id` 等标识。
* pairing、auth、session、control、resize、E2EE frame 的 payload。

协议类型不包含 UI 逻辑，也不表达账号体系或平台策略。

### `termd`

`termd` 是服务器 daemon，当前职责包括：

* PTY session runtime。
* session 状态机：`CREATED -> RUNNING -> CLOSED`。
* controller/viewer 控制权规则。
* 设备级 pairing/auth 协议边界。
* X25519 + HKDF + ChaCha20Poly1305 的 E2EE `encrypted_frame`。
* HTTP `/healthz` 和 WebSocket `/ws`。
* 本地 `/local/pairing-token` token 签发入口。
* outbound relay connector：`--relay ws://host:port` 会连接 relay 的 `/daemon-mux`；支持重复传入多个 `--relay` / `--relay-url` 端点，daemon 会为每个 endpoint 启动独立 supervisor。

当前启动入口是：

```bash
cargo run -p termd
```

常用命令：

```bash
cargo run -p termd -- pair
cargo run -p termd -- --listen 127.0.0.1:8765
cargo run -p termd -- --relay ws://127.0.0.1:8080
```

默认配置监听 `127.0.0.1:8765`，systemd/docker 部署可以通过 `--listen host:port` 覆盖监听地址。配置和 state 模块已经存在，完整配置文件入口仍保持 MVP 范围。

### `termctl`

`termctl` 是 WebSocket CLI，既可以直连 `termd`，也可以通过 relay client URL 连接真实 daemon。当前真实命令是：

```text
pair
new
attach
control
resize
list
```

全局参数：

```text
--state <PATH>
```

默认 daemon URL：

```text
ws://127.0.0.1:8765/ws
```

`termctl` 保存本地设备 key 和已配对 server 信息，但不保存 pairing token、server private key 或终端业务明文。

### `termrelay`

`termrelay` 是 dumb pipe，当前启动入口是：

```bash
cargo run -p termrelay -- --listen 127.0.0.1:8080
```

参数：

```text
--listen|-l <SocketAddr>
```

默认监听：

```text
127.0.0.1:8080
```

WebSocket 路径：

```text
/ws/{server_id}/daemon
/ws/{server_id}/daemon-mux
/ws/{server_id}/client
```

relay 只按 URL 中公开的 `server_id` 路由 frame。`/daemon-mux` 只解析 relay transport wrapper，用于 `client_id` 定向转发；它不解密、不解析内层业务 envelope，也不参与 pairing/auth/session/control 判断。
公网部署方案、反向代理示例和 health check 细节见 [docs/deployment.md](docs/deployment.md)。

### 发布与安装

* workspace package 版本与 Git tag 保持一致，release 资产和 GHCR 镜像都使用同一个 tag。
* `termctl` 和 `termd` 提供 curl/wget 安装脚本；`termd` 安装脚本默认注册 systemd 服务。
* `termrelay` 提供 systemd 安装脚本，另外还有 `deploy/termrelay/docker-compose.yml` 的容器化部署方式。
* GitHub Actions 在 tag push 时会同时构建 release tarball、发布 GitHub Release 资产，并推送 `ghcr.io/<owner>/<component>:<tag>` 镜像。

### `termui/frontend`

Web MVP 使用 React、TypeScript、Vite、Vitest 和 Playwright。当前能力包括：

* pairing token consumer。
* direct/relay WebSocket URL 输入。
* session list、attach、controller/viewer 状态展示和 control request。
* IndexedDB 设备状态存储边界测试。
* Playwright 覆盖 mock daemon 和真实 `termrelay + termd --relay` pairing/list。

Web 不提供 daemon 侧 token 签发 UI，不保存 pairing token、server private key 或 terminal transcript。

### `termui/native`

Flutter Native 目前是架构骨架，包含 app/features/core/service/storage/protocol 分层和安全边界测试。当前环境没有 Flutter/Dart SDK 时，`scripts/qa.sh` 只运行 fallback 结构和敏感字符串检查，不能把它视为完整 Native client。

---

## 当前数据流

### Direct 模式

```text
termctl
  |
  | ws://127.0.0.1:8765/ws
  v
termd
```

direct 模式中，`termctl` 先完成 E2EE key exchange，再把 pairing/auth/session/control 等业务 envelope 放入 `encrypted_frame`。

### Relay 模式边界

```text
termd --relay      -> termrelay /ws/{server_id}/daemon-mux
termctl / Web      -> termrelay /ws/{server_id}/client
```

relay runtime E2E 覆盖真实 `termrelay` 进程、真实 `termd --relay` 进程，以及 `termctl pair/new/list` 经 relay client URL 访问 daemon。Web E2E 也覆盖浏览器经真实 relay client URL 完成 pairing/list。

---

## MVP 状态矩阵

| 项目 | 当前状态 |
| --- | --- |
| PTY 创建与 I/O 桥接 | 已实现并测试 |
| client 断开后 session 不立即终止 | 已实现并通过 E2E 验证 |
| 第一个 attach 成为 controller | 已实现并测试 |
| 后续 attach 成为 viewer | 已实现并测试 |
| 已配对设备抢占 controller | 已实现并测试 |
| pairing token 生命周期 | 协议/API 已实现 |
| 用户命令签发 pairing token | 已实现 `termd pair` 本地 token 签发 |
| 二维码/扫码 pairing | 已实现并验证 |
| challenge-response auth | 已实现并测试 |
| replay protection | 已实现并测试 |
| E2EE `encrypted_frame` | 已实现并测试 |
| Noise protocol | 未实现；当前不是 Noise handshake |
| direct `termd` WebSocket 服务 | 已实现并通过 E2E 验证 |
| `termctl pair/new/attach/control/resize/list` | 已实现并通过 E2E 验证 |
| `termrelay` dumb pipe | 已实现并通过 E2E 验证 |
| daemon 自动连接 relay | 已实现 `termd --relay` 本地 MVP，并通过 runtime E2E 验证 |
| `termui/frontend` Web MVP | 已实现 pairing/list/attach/control 的 MVP，并通过 mock daemon 与真实 relay E2E 验证 |
| `termui/native` Flutter | 架构骨架，非完整 client |
| 多 relay 配置 | 已实现 |
| 公网部署方案、反向代理与运维文档 | 已实现 |
| 个人使用定位 | 已明确 |

---

## UI 边界

`termui/frontend` 当前已是可用 Web MVP，但 UI 仍必须遵守这些边界：

* UI 只做 pairing token consumer，不签发 daemon token。
* UI 不保存 server private key、pairing token 或 terminal transcript。
* UI 不实现 daemon 业务逻辑，不判断 controller/viewer 控制权。
* UI 不把 relay mux transport 细节写入展示层；真实 relay 测试通过用户可配置 WS URL 走现有 DirectClient 行为。

`termui/native` 仍是后续完善方向，当前只验证架构和安全边界。

---

## 验证入口

推荐先运行完整 workspace 测试：

```bash
bash scripts/qa.sh
```

当前关键 E2E 覆盖：

* `termctl/tests/direct_daemon_e2e.rs`：真实 `termctl` binary 连接 in-process `termd` daemon，覆盖 pair/new/list/attach/control/resize 和断线后 session 仍运行。
* `termrelay/tests/relay_e2e.rs`：真实 `termrelay` binary 转发 encrypted frame，覆盖 relay 不见业务明文和 `server_id` 隔离。
* `scripts/qa.sh` runtime relay E2E：启动真实 `termrelay` 和 `termd --relay`，通过 relay client URL 运行 `termctl pair/new/list`。
* `termui/frontend/tests/termui-web.real-relay.spec.ts`：浏览器通过真实 relay client URL 连接 daemon 完成 pairing/list。
