# QA 矩阵

`bash scripts/qa.sh` 是当前 MVP 的统一本地 QA 入口。脚本不依赖旧 controller/viewer 模式，不安装系统依赖，也不写 checklist。

## 覆盖范围

| 范围 | 命令 |
| --- | --- |
| shell 脚本语法 | `bash -n scripts/*.sh` |
| Rust 格式 | `cargo fmt --all -- --check` |
| Rust workspace | `cargo test --workspace --locked` |
| pairing CLI E2E | 启动本地 `termd`，运行 `termd pair --qr` 获取 `termd-pair:v2` 邀请码，再运行 `termctl pair --payload` 完成设备配对 |
| termctl direct E2E | `cargo test -p termctl --test direct_daemon_e2e` |
| termrelay open-relay E2E | `cargo test -p termrelay --test relay_e2e`；binary 使用 `--allow-open-relay`，覆盖路由、明文/二进制 frame 转发、旧 WebSocket `encrypted_frame` 兼容和 `server_id` 隔离，不覆盖 trusted admission |
| trusted relay admission | `termrelay/src/ws.rs` 单元测试覆盖 daemon token、pair ticket、device admission 与 replay；`scripts/qa.sh` 启动真实 `termrelay` 和 `termd --relay` 覆盖真实 trusted admission/routing |
| 安装脚本 smoke | `bash scripts/test-installers.sh`，检查三个安装脚本的帮助和 systemd 语义 |
| termui Web | `npm ci`、`npm run typecheck`、`npm run test -- --run`、`npm run build`、`npm run test:e2e`、`npm audit --audit-level=high`；Playwright 覆盖 mock daemon 和真实 relay daemon |
| termui Native | 有 Flutter/Dart 时运行 `flutter pub get`、`flutter analyze`、`flutter test`；缺失时运行结构和敏感字符串 fallback 检查。 |

## 前端依赖安装

`scripts/qa.sh` 默认每次都会在 `termui/frontend` 运行 `npm ci`，保证本地 QA、PR CI 和发版验证使用 `package-lock.json` 固定的依赖树。

只有在明确知道当前 `node_modules/` 已由同一个 lockfile 安装、且需要离线或加速复跑时，才可以显式跳过：

```bash
TERMD_QA_SKIP_NPM_CI=1 bash scripts/qa.sh
```

不要依赖 `node_modules/` 是否存在来隐式跳过安装。

## 公网部署 smoke QA

- 确认 `wss://relay.example/ws` 可以完成 pair / new / list，`server_id` 只出现在连接后的 `route_hello` 中。
- 确认反向代理保留 WebSocket upgrade，并且 setup token、daemon token 或旧 `relay_token` 不出现在 access log 或 error log。
- 确认 `termrelay /healthz` 可从公网 health check 入口访问，而 `termd /healthz` 仍留在私网或 loopback。
- 确认 `termd /local/pairing-token` 不能从公网入口访问。
- 确认 relay 按可信 admission/routing 层部署：TLS 终止后可见明文 WebSocket/HTTP tunnel 应用流量，但 pairing、auth 和 session 权限仍由 daemon 最终校验。

## 已知非阻断项

- `termctl` 测试构建可能输出 test helper `dead_code` warning；测试通过时不阻断交付。
- Vite build 可能输出 chunk size warning；构建成功时不阻断交付。
- 当前环境如果没有 Flutter/Dart，必须记录未运行 `flutter analyze/test/build`，不能把 Native 写成完整验证通过。

## 生成物边界

`node_modules/`、`dist/`、`test-results/`、`playwright-report/`、`.dart_tool/`、`build/` 和 `coverage/` 是本地验证副产物，不作为交付源码审查。
