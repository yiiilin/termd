# WebSocket Writer Queue Backpressure

> 历史状态提示：本文记录当时的计划/实现状态，不代表当前 0.6 协议契约；现行边界以 `TECH.md` 和 `docs/deployment.md` 为准。

## Goal

把 direct 和 relay 的终端输出链路收敛成：

```text
bounded writer queue accepted = 当前连接输出责任已交给 transport
writer socket send failed = 当前连接失败并关闭/重连
```

不再用成功 outcome 结算每次 socket write，也不再用 inflight/relay data backlog 追踪“真实 send 已完成”。

## Execution Rule

Progress must be tracked in this file only:

- Start every pending task as unchecked
- Mark each task with `[x]` immediately after implementation and verification complete
- If a task reveals a prerequisite gap, add a new unchecked task directly below it before continuing
- If any task remains unchecked, the project is not complete

## Tasks

- [x] 盘点并删除 direct 成功 outcome / inflight after-send 依赖，保留 writer failure signal
- [x] 盘点并删除 relay 成功 outcome / relay data backlog 依赖，保留 writer failure signal
- [x] 更新 direct/relay 相关单测到 queue accepted 背压模型
- [x] 跑定向测试、全量 termd/termrelay 测试和 release build
