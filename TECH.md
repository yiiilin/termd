## 技术架构

### Rust 系统组件

以下组件使用 Rust 实现：

- `termd`：服务器 daemon
- `termctl`：CLI 管理工具
- `termrelay`：relay 中继服务

Rust 负责系统层能力，包括 PTY 管理、WebSocket 通信、端到端加密、session 生命周期、relay 路由、本地状态和 CLI 操作。

推荐技术栈：

- async runtime：Tokio
- HTTP / WebSocket：Axum
- CLI：Clap
- 序列化：Serde
- 日志：Tracing
- 配置：TOML / YAML
- 本地状态：SQLite / redb
- 加密：Noise / X25519 / Ed25519
- PTY：portable-pty / nix

### Native GUI 客户端

`termui` 的桌面端与移动端使用 Flutter 实现：

- macOS / Windows / Linux
- iOS
- Android

Flutter 负责：

- pairing 扫码 / 粘贴
- session 列表
- 终端视图
- 多标签页
- 控制权切换
- 连接状态展示
- 本地设备密钥存储

推荐技术栈：

- Riverpod
- go_router
- web_socket_channel
- mobile_scanner
- qr_flutter
- flutter_secure_storage
- xterm.dart 或自研 terminal widget

### Web 客户端

`termui-web` 使用主流 Web 技术独立实现：

- React / Next.js
- TypeScript
- xterm.js
- WebSocket API
- Tailwind CSS
- Zustand / TanStack Query

Web 端重点负责：

- 快速访问 session
- 浏览器终端体验
- pairing code 粘贴
- relay 连接
- session attach / reconnect

### 技术边界

Rust 组件负责：

- 协议
- 加密
- PTY
- session
- relay
- 本地状态

Flutter / Web 组件负责：

- UI
- 用户交互
- 配对流程
- 终端渲染
- 本地 client device key 管理

客户端不得实现 daemon 业务逻辑；所有客户端必须通过统一协议与 `termd` 通信。
