# 恶劣网络韧性设计

> 目标：把 termd 的 direct / relay 使用体验从“低延迟稳定网络优先”调整为“弱 timeout、强恢复、terminal 优先”。在高 RTT、抖动、低带宽、浏览器后台冻结和 relay 中转排队场景下，只要 WebSocket 没有真实断开，terminal 链路就不应因为普通 RPC 超时或队列短暂承压而失败。

## 1. 背景

当前实现已经收敛到单 WebSocket segment 架构，但仍然残留两个对差网络不友好的假设：

1. 普通 request/response 使用固定短 timeout，超时会被提升成连接级错误。
2. 发送队列、连接恢复和 UI 状态机仍把“慢”与“坏”混在一起处理。

实际使用时，网络可能出现：

- RTT 300 ms 到 1000 ms。
- 短时间带宽跌到 1 Mbps 以下。
- relay 出现几秒到几十秒排队。
- 浏览器切后台后 JS 定时器冻结，恢复时旧请求集中返回。
- WebSocket 仍保持连接，但部分普通 RPC 比 terminal 输出慢很多。

因此新的模型必须把“连接是否存活”和“某次操作是否及时返回”彻底解耦。

## 2. 设计原则

1. **连接存活只由 WebSocket close/error 决定。**
   普通 RPC timeout 不关闭 workspace WebSocket，也不裁定 relay / daemon 离线。

2. **terminal 优先。**
   terminal snapshot/stdout/stdin/resize 是主数据流。files/git/status/clients 等非 terminal 操作慢了只能影响自己的 UI 状态，不能卸载 xterm 或重建连接。

3. **timeout 是 UI 期限，不是传输失败。**
   对用户可见的按钮操作可以显示“仍在等待”或“本次操作超时”，但底层请求可以被取消、忽略迟到响应，不能因此关闭 socket。

4. **背压优先于失败。**
   terminal 输出进入 writer queue 后，由 WebSocket/TCP 负责承压。队列短暂满时等待容量；只有连接关闭或长期无法写入才关闭该 browser 连接。

5. **恢复靠 snapshot。**
   browser、relay 或网络发生真实断开后，browser 重新接入，daemon 用 mirror snapshot + tail 恢复 terminal，而不是依赖旧请求继续有效。

6. **relay 保持 dumb pipe。**
   relay 不解析 terminal、request、session 或业务错误。它只路由 opaque frame，按连接 close/error 释放状态。

## 3. 连接状态模型

连接级状态只关心：

```text
connecting -> open -> closing -> closed
```

普通 RPC 状态只关心：

```text
pending -> completed | ui_timeout | canceled | stale
```

两者不能互相升级：

- `ui_timeout` 不会把 `open` 改成 `closed`。
- `canceled/stale` 只丢弃本地等待者，不发送连接级错误。
- 只有 socket close/error 才进入 `closed` 并触发重连。

## 4. Timeout 策略

保留硬 timeout：

- WebSocket connect。
- route prelude。
- E2EE 握手。
- pairing bootstrap。

改成软 timeout：

- session.list。
- daemon.status。
- daemon.clients。
- files/git/search。
- 普通 ping latency 测量。

软 timeout 的行为：

1. request 等待者按 UI deadline 返回 `response_timeout`。
2. request id 保留一个短期 stale record，迟到 response 到达时被丢弃并记录 debug。
3. socket 不关闭。
4. 当前 terminal stream 不受影响。

terminal attach 使用较长连接 deadline，但一旦 workspace WebSocket 已 open，terminal stream 的数据接收不再依赖普通 request timeout。

## 5. 队列和背压

### Browser

- `DirectClient` inbound queue 只做本地消息缓冲。
- 非 terminal event 可以合并或丢旧，例如 daemon.status 只保留最新。
- terminal frame/session_data 必须按序投递给 TerminalPane。

### Daemon Direct

- writer queue accepted 是输出责任边界。
- queue 满时等待容量，不提前消费 session output cache。
- 等待容量不应使用很短 timeout。
- 只有 writer 任务收到 socket send error / close，才清理该 client。

### Relay

- relay 对 daemon/browser 的 frame 使用连接级背压。
- data/control 可以有内部优先级，但不能把 data queue 满解释为业务失败。
- ping/pong 只用于保活，不用于业务在线判断；连接断开才离线。

## 6. 恢复路径

### Browser 断开或后台恢复

```text
old websocket closed
  -> browser reconnect workspace websocket
  -> auth
  -> session.list
  -> terminal.attach(current session)
  -> snapshot + tail
```

旧 request 全部视为 stale。旧 terminal stream 由 daemon 在连接断开时清理，session 继续运行。

### Relay 断开

daemon 到 relay 的长连接断开时，relay 认为 daemon 离线；daemon 后台继续保持 session。daemon 重连 relay 后，browser 再连入时走正常 workspace attach。

### 普通 RPC 慢

普通 RPC 超过 UI deadline：

- UI 显示该区域 stale / retry。
- 请求迟到时如果仍匹配当前 server/session generation，可选择应用。
- 不关闭 terminal。

## 7. 测试基线

必须覆盖：

1. direct 下 session.list 超过 5 秒，但 terminal 已 attach 时 xterm 不卸载。
2. relay 下 route/E2EE 建立后，daemon.status 超时不关闭 workspace WebSocket。
3. 高 RTT / 慢 session.list 后迟到 response 不覆盖新 daemon/session。
4. writer queue 满时 push drain 等待容量，不丢事件、不忙等。
5. 两个客户端同 session 持续输出时，一个客户端慢不影响另一个客户端输入。
6. browser hidden 后恢复，旧请求 stale，新 attach 能通过 snapshot 恢复。
7. relay 只在连接 close/error 后释放连接，不用业务 timeout 判定离线。

## 8. 不变式

1. relay 不解析 E2EE 明文。
2. 普通 RPC timeout 不关闭 workspace WebSocket。
3. terminal stream 不等待 daemon.status/files/git。
4. terminal 输出不因 browser 渲染慢而停止 PTY。
5. 真实断线后的恢复路径必须是 reconnect + snapshot + tail。
6. stale request 不能覆盖当前 daemon/session generation。

## 9. 实施顺序

1. 前端 `DirectClient` 支持软 timeout / stale response。
2. App 状态机把普通 RPC timeout 降级为局部错误。
3. daemon direct writer queue 去掉短写入 timeout 或改成长连接背压。
4. relay writer queue 保持 dumb pipe，只在 socket close/error 时失败。
5. 补恶劣网络测试和回归测试。

