# Web Push Background Notifications

## Goal

在浏览器页面被系统冻结或关闭后，仍能通过标准 Web Push 收到 termd AI session 的低频状态通知，并可从通知恢复到对应 daemon/session。

## Confirmed Scope

- 只通知 AI activity 的 `attention`，以及从 `running` 进入 `completed` 或 `idle`
- 不发送 PTY 输出、终端输入、文件内容、access token 或任何私钥
- 页面可见时由 Service Worker 抑制系统通知
- 点击通知选择对应 daemon、进入 workspace 并 attach 对应 session
- relay 只扩展受限 HTTP tunnel，不保存订阅、VAPID 密钥或通知业务状态
- 本轮只实现和本地验证，不发版、不部署

## Design

### Deep Module

`PushNotificationCoordinator` 是 daemon 内的深模块，其小接口负责：

- 持久生成和读取每个 daemon 的 P-256 VAPID identity
- 按已认证 `device_id` 保存、更新和删除 Push subscription
- 从 metadata activity snapshot 建立 baseline、检测目标状态转换并去重
- 通过 `PushDelivery` seam 加密并发送通知
- 对 `404/410` endpoint 自动清理，对网络错误和临时 HTTP 错误做 best-effort 处理

生产 adapter 使用 `web-push-native` 构造标准 VAPID/ECE 请求并由现有 `reqwest` 发送；测试 adapter 只观察模块接口，不接触网络。

### Persistence

在 daemon 现有 SQLite 文件中增加独立 additive 表，不改变 `DaemonState` 快照版本：

- `web_push_identity`: 单行持久 VAPID 公私钥
- `web_push_subscriptions`: `device_id`, endpoint, `p256dh`, auth secret, mode, locale 和更新时间

订阅不对 `trusted_devices` 建级联外键，因为现有状态保存会重写可信设备表。HTTP handler 只使用 Bearer token claims 中的 `device_id`，不信任请求体设备标识。

### HTTP Interface

```text
GET    /api/push/config
PUT    /api/push/subscription
DELETE /api/push/subscription
```

- 所有接口都要求 daemon 验证短期 Bearer access token
- `config` 返回 base64url VAPID public key 和 server id
- subscription 输入限制大小、要求 HTTPS endpoint，并只允许已知 mode/locale
- `DELETE` 携带浏览器当前 endpoint；daemon 仅在设备与 endpoint 同时匹配时删除，
  避免迟到的退订请求删除重新配对后创建的新 subscription
- relay/proto allowlist 精确允许上述 method/path，并补充 CORS `GET/PUT/DELETE`

### Browser Lifecycle

- 每个 `server_id` 使用独立 Service Worker scope，避免同一 relay origin 下多个 daemon 的 application server key 冲突
- 前端在用户开启通知并获得权限后注册 worker、创建 subscription 并同步 daemon
- 关闭通知时同时从 daemon 删除 subscription、调用 browser `unsubscribe()` 并注销该 scope
- Service Worker 不注册 `fetch` handler，不缓存应用资源，只处理 `install`、`activate`、`push` 和 `notificationclick`
- Push payload 只含版本、server/session 标识、session 展示名、agent/state 和目标 URL
- Web Push 和 Service Worker 要求 secure context；浏览器通常仅对 HTTPS 开放，
  `localhost`/`127.0.0.1` 具有本地开发例外，普通 `http://192.168.x.x` 直连不支持
- iOS/iPadOS 支持范围为 16.4+，且必须通过 HTTPS 访问并已将网页添加到主屏幕

## Test Seams

- `PushNotificationCoordinator`: persistence、baseline、transition 去重、payload 限制、失效 endpoint 清理
- authenticated `/api/push/*`: auth、输入验证、设备归属和响应契约
- proto/relay HTTP tunnel: exact allowlist、method、CORS 和透明转发
- frontend Push module + Service Worker: scoped registration、订阅同步、关闭清理、push 抑制和 notification click URL
- App restore flow: notification URL 选择 daemon 并恢复 session

## Security And Failure Rules

- VAPID 私钥、Push endpoint、`p256dh`、auth secret 和 Bearer token不得进入日志
- Push endpoint 仅允许 HTTPS，禁止 redirect，设置请求超时和输入长度上限
- 通知投递不得在持有 `DaemonProtocol` mutex 时等待网络
- observer 启动只建立 baseline，不为历史状态补发通知
- Push 通道故障不得影响 terminal、session、metadata 或 daemon 启动后的主链路

## Rollback / Stop Conditions

- 前端可通过关闭通知退订；删除 Service Worker 注册不会影响 IndexedDB 配对状态
- SQLite 变更仅新增表，旧二进制会忽略它们
- 若依赖无法在现有 rustls provider 下稳定构建，停止在 delivery adapter 之前并重新评估，不替换 daemon TLS 栈
- 若 relay 需要持有任何通知业务状态，停止实施并重新设计

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Tasks

- [x] Add the daemon VAPID/subscription persistence module with restart and ownership tests
- [x] Add authenticated daemon Push HTTP interfaces plus proto/relay allowlist and routing tests
- [x] Add activity transition coordination, encrypted delivery, invalid-subscription cleanup, and failure-isolation tests
- [x] Replace the cleanup-only worker with scoped Push lifecycle handling and focused frontend tests
- [x] Integrate preference synchronization and notification-click daemon/session restoration without page-only duplicates
- [x] Run focused and full Rust/frontend verification, review security-sensitive diff, and document residual platform limits
- [x] Degrade to disabled Push when optional Push state initialization fails without blocking daemon startup
- [x] Clean the remote subscription and scoped worker before forgetting a daemon
- [x] Deliver every attention transition and replace coalescing watch snapshots with a bounded typed activity-event path
- [x] Close the Service Worker activation listener race
- [x] Re-run focused and full verification after review fixes
- [x] Preserve combined-frame activity transitions and explicit activity clear events
- [x] Bound startup activity-event draining to the queue length observed on entry
- [x] Make delayed subscription deletion conditional on the browser's previous endpoint
- [x] Re-run full verification after final race fixes
