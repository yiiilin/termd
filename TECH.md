# 技术架构

本文记录当前代码库的真实技术状态。当前 Rust workspace 包含 `proto`、`termd`、`termctl`、`termrelay` 和 `termweb`；`termui/frontend` 是已验证的 Web MVP，`termui/native` 是 Flutter Native 架构骨架。

---

## 当前 Rust Workspace

```text
/proto
/termd
/termctl
/termrelay
/termweb
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
* tmux-backed terminal frame 的 snapshot/output/resize/exit 语义。

协议类型不包含 UI 逻辑，也不表达账号体系或平台策略。

### `termd`

`termd` 是服务器 daemon。当前生产默认 backend 已切到 tmux host runtime；这一刀的边界是：

* tmux session host runtime：tmux 是持久终端会话守护程序。
* daemon runtime 为每个 termd session 维护一个 `tmux attach-session` PTY bridge，并沿用现有协议层 fanout 分发给多个已 attach Web client。
* `watch_updates=true` 的 Web attach 会在 runtime 中创建独立、短生命周期的 control-mode tmux client handle；terminal stream 取消或连接断开时只释放本连接自己的 handle，不终止 tmux session host。

当前保留职责：

* session 状态机：`CREATED -> RUNNING -> CLOSED`。
* shared-control operator 规则。
* 设备级 pairing/auth 协议边界。
* X25519 + HKDF + ChaCha20Poly1305 的 E2EE `encrypted_frame`。
* HTTP `/healthz` 和 WebSocket `/ws`。
* 本地 `/local/pairing-token` token 签发入口。
* outbound relay connector：`--relay ws://host:port` 会连接 relay；daemon 只把加密业务 frame 交给 relay 转发。

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

默认配置监听 `127.0.0.1:8765`，systemd/docker 部署可以通过 `--listen host:port` 覆盖监听地址。tmux-backed state 使用新的 schema version；旧 supervisor restore state 不迁移。

### `termctl`

`termctl` 是 WebSocket CLI 调试工具，既可以直连 `termd`，也可以通过 relay client URL 连接真实 daemon。tmux-backed 架构下正式交互客户端是 Web；`termctl attach` 不再驱动产品语义。当前保留命令是：

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
/ws
```

relay 在统一 `/ws` 上只解析连接前奏 `route_hello`，用其中的 `server_id`、`role`、`route_generation` 和 data pipe token 完成路由绑定。旧 path-based relay 路由不再作为当前入口；relay 建链后只转发 opaque WebSocket frame，不解密、不解析内层业务 envelope，也不参与 pairing/auth/session/control 判断。
公网部署方案、反向代理示例和 health check 细节见 [docs/deployment.md](docs/deployment.md)。

### `termweb`

`termweb` 是 Rust workspace 内的嵌入式 Web 静态资源 crate。发布构建会把 `termui/frontend/dist` 嵌入 `termd` 和 `termrelay` 二进制；本地未构建前端时，build script 会嵌入最小占位页，保证 Rust workspace 测试和构建不依赖前端产物已经存在。

`termweb` 只负责静态资源响应和 SPA fallback，不保存 UI 状态，不处理 pairing/auth/session/control 业务协议。

### 发布与安装

* workspace package 版本与 Git tag 保持一致，release 资产和 GHCR 镜像都使用同一个 tag。
* `termctl` 和 `termd` 提供 curl/wget 安装脚本；`termd` 安装脚本默认注册 systemd 服务。
* 当前 `termd.service` 使用状态目录内的 tmux socket 承载 session host；旧 supervisor runtime 不再被生产启动路径自动 adopt。`termrelay` 也提供 systemd 安装脚本，另外还有 `deploy/termrelay/docker-compose.yml` 的容器化部署方式。
* GitHub Actions 在 tag push 时会同时构建 release tarball、发布 GitHub Release 资产，并推送 `ghcr.io/<owner>/<component>:<tag>` 镜像。

### `termui/frontend`

Web MVP 使用 React、TypeScript、Vite、Vitest 和 Playwright。当前能力包括：

* pairing token consumer。
* direct/relay WebSocket URL 输入。
* session list、attach、shared-control 状态展示和旧 control request noop。
* IndexedDB 设备状态存储边界测试。
* Playwright 覆盖 mock daemon 和真实 `termrelay + termd --relay` pairing/list。
* 终端渲染通过 `TerminalRendererAdapter` 接入 renderer；默认仍使用 xterm，设置 `VITE_TERMD_TERMINAL_RENDERER=ghostty` 时动态加载 `ghostty-web` WASM renderer。`TerminalPane` 不直接依赖 renderer 私有 DOM。

Web 不提供 daemon 侧 token 签发 UI，不保存 pairing token、server private key 或 terminal transcript。

### `termui/native`

Flutter Native 目前是架构骨架，包含 app/features/core/service/storage/protocol 分层和安全边界测试。当前环境没有 Flutter/Dart SDK 时，`scripts/qa.sh` 只运行 fallback 结构和敏感字符串检查，不能把它视为完整 Native client。

---

## tmux-backed 当前数据流

### Direct 模式

```text
Web
  |
  | ws://127.0.0.1:8765/ws
  v
termd
  |
  | daemon runtime attach PTY bridge
  v
tmux session
```

tmux-backed direct 模式中，Web 先完成 E2EE key exchange，再把 pairing/auth/session/control/file/terminal 等业务 envelope 放入 `encrypted_frame`。当前 terminal 明文仍由 daemon runtime attach PTY bridge 读取，并通过 termd 协议层 session 级 frame log / fanout 分发；watched Web attach 额外拥有独立 control-mode tmux client lifecycle handle，用于把 Web watcher 生命周期映射到真实 tmux client，但不改变 session 级 `terminal_seq` 输出语义。

### Relay 模式边界

```text
termd --relay      -> termrelay /ws + route_hello(role=daemon_control/daemon_data)
Web                -> termrelay /ws + route_hello(role=client)
```

relay runtime E2E 覆盖真实 `termrelay` 进程、真实 `termd --relay` 进程，以及 Web 经 relay client URL 访问 daemon。relay 不解密、不解析 session/file/tmux 业务。

---

## MVP 状态矩阵

| 项目 | 当前状态 |
| --- | --- |
| tmux session host | 已实现第一刀，当前生产默认 backend 创建 tmux session |
| daemon runtime attach PTY bridge | 已实现，用 `tmux attach-session` 连接 tmux host 并复用现有协议层 fanout |
| watched Web attach tmux client lifecycle | 已实现，`watch_updates=true` 创建独立 control-mode tmux client handle，stream cancel/connection close 释放自己的 handle |
| PTY 创建与 I/O 桥接 | 已实现，后续降级为 attach transport |
| client 断开后 session 不立即终止 | 已实现并通过 E2E 验证 |
| attach 后成为 operator | 已实现并测试 |
| 多客户端 shared-control | 已实现并测试 |
| `control_request` 旧命令 noop | 已实现并测试 |
| pairing token 生命周期 | 协议/API 已实现 |
| 用户命令签发 pairing token | 已实现 `termd pair` 本地 token 签发 |
| 二维码/扫码 pairing | 已实现并验证 |
| challenge-response auth | 已实现并测试 |
| replay protection | 已实现并测试 |
| E2EE `encrypted_frame` | 已实现并测试 |
| Noise protocol | 未实现；当前不是 Noise handshake |
| direct `termd` WebSocket 服务 | 已实现并通过 E2E 验证 |
| `termctl pair/new/attach/control/resize/list` | 旧 CLI 能力；tmux-backed 架构下交互 attach 降级为调试 |
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
* UI 不实现 daemon 业务逻辑，不判断 shared-control operator 规则。
* UI 不把 relay mux transport 细节写入展示层；真实 relay 测试通过用户可配置 WS URL 走 Web protocol client 行为。
* UI 通过 `TerminalRendererAdapter` 使用 Ghostty/xterm 兼容渲染器，不能依赖 renderer 私有 DOM 作为业务语义；xterm 私有 textarea 锚定逻辑只能留在 xterm adapter 内部。

`termui/native` 仍是后续完善方向，当前只验证架构和安全边界。

---

## 验证入口

推荐先运行完整 workspace 测试：

```bash
bash scripts/qa.sh
```

当前关键 E2E 覆盖：

* `termctl/tests/direct_daemon_e2e.rs`：旧 CLI 调试路径，覆盖 pair/new/list/attach/control/resize 的兼容行为。
* `termrelay/tests/relay_e2e.rs`：真实 `termrelay` binary 转发 encrypted frame，覆盖 relay 不见业务明文和 `server_id` 隔离。
* `scripts/qa.sh` runtime relay E2E：启动真实 `termrelay` 和 `termd --relay`，覆盖 relay dumb pipe 和客户端路由。
* `termui/frontend/tests/termui-web.real-relay.spec.ts`：浏览器通过真实 relay client URL 连接 daemon 完成 pairing/list。
