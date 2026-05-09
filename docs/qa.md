# QA 矩阵

`bash scripts/qa.sh` 是当前 MVP 的统一本地 QA 入口。脚本不依赖旧 controller/viewer 模式，不安装系统依赖，也不写 checklist。

## 覆盖范围

| 范围 | 命令 |
| --- | --- |
| Rust 格式 | `cargo fmt --all -- --check` |
| Rust workspace | `cargo test --workspace` |
| pairing CLI E2E | 启动本地 `termd`，运行 `termd pair` 获取 token，再运行 `termctl pair --token` 完成设备配对 |
| termctl direct E2E | `cargo test -p termctl --test direct_daemon_e2e` |
| termrelay E2E | `cargo test -p termrelay --test relay_e2e` |
| relay runtime E2E | 启动本地 `termrelay` 和 `termd --relay`，通过 relay client URL 运行 `termctl pair/new/list` |
| termui Web | `npm run typecheck`、`npm run test -- --run`、`npm run build`、`npm run test:e2e`、`npm audit --audit-level=high`；Playwright 覆盖 mock daemon 和真实 relay daemon |
| termui Native | 有 Flutter/Dart 时运行 `flutter pub get`、`flutter analyze`、`flutter test`；缺失时运行结构和敏感字符串 fallback 检查。 |

## 公网部署 smoke QA

- 确认 `wss://relay.example/ws/{server_id}/client?relay_token=...` 可以完成 pair / new / list。
- 确认反向代理保留 WebSocket upgrade，并且 `relay_token` 不出现在 access log 或 error log。
- 确认 `termrelay /healthz` 可从公网 health check 入口访问，而 `termd /healthz` 仍留在私网或 loopback。
- 确认 `termd /local/pairing-token` 不能从公网入口访问。
- 确认 `termctl` 与 Web 仍然只把 relay 当作 transport，不把 relay 当成可信业务层。

## 已知非阻断项

- `termctl` 测试构建可能输出 test helper `dead_code` warning；测试通过时不阻断交付。
- Vite build 可能输出 chunk size warning；构建成功时不阻断交付。
- 当前环境如果没有 Flutter/Dart，必须记录未运行 `flutter analyze/test/build`，不能把 Native 写成完整验证通过。

## 生成物边界

`node_modules/`、`dist/`、`test-results/`、`playwright-report/`、`.dart_tool/`、`build/` 和 `coverage/` 是本地验证副产物，不作为交付源码审查。
