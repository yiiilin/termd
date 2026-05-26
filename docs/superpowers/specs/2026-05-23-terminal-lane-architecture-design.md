# 单 WebSocket Segment 架构设计

> 目标：把 browser 到 daemon 的连接语义固定为“一条可靠 WebSocket 逻辑流”。terminal 和非 terminal 只是 E2EE 内层 segment 类型；direct 与 relay 只改变传输路径，不改变 daemon 的 client controller 语义。

## 1. 背景

termd 的核心目标是像 `sshd + tmux` 一样提供持久终端。历史实现曾尝试拆出 terminal / aux 多条 WebSocket，并用 ACK / credit 或额外连接边界解决大输出、快速切换、relay 卡顿等问题。这个方向会引入新的复杂度：

1. browser 切换 session 时，多条 WebSocket 的生命周期容易互相误伤。
2. relay 会被迫感知 stale 连接、terminal flow 等业务语义，偏离 dumb pipe 原则。
3. terminal 与 files/git/status 等操作其实都属于同一个已认证 browser client，多线会放大状态同步成本。
4. WebSocket/TCP 已经提供可靠有序传输，应用层 ACK 不应成为输出卡死的条件。

最终模型收敛为：外层只保留一条可靠 WebSocket；E2EE 明文内使用 `ProtocolPacket` 作为 segment/batch 复用层。

## 2. 设计结论

1. browser 对一个 daemon 只保留一条 active workspace WebSocket。
2. pairing 可以使用一次性 bootstrap 连接；配对完成后关闭。
3. terminal attach、snapshot/stdout、stdin、resize、session.list、daemon.status、files、git 等都复用 workspace WebSocket。
4. 业务分类只发生在 E2EE 内层 segment：`request`、`response`、`event`、`stream_open`、`stream_chunk`、`stream_end`、`cancel`、`error`。
5. terminal stream 是这条 WebSocket 上的一个 `stream_id`，切换 session 时发送 `cancel` 取消旧 stream，再 `stream_open terminal.attach` 打开新 stream。
6. relay 是 dumb pipe：按 `server_id` 路由并转发 opaque binary frame，不解密、不解析 session、不判断 terminal/非 terminal。
7. terminal 输出不等待 browser render ACK，也不使用应用层 credit；背压由 WebSocket/TCP、daemon bounded queue 和慢连接关闭处理。

## 3. 总体架构

```text
Direct:

Browser
  |-- workspace websocket
  |     E2EE ProtocolPacket segments
  v
daemon http/ws controller
  -> daemon client controller
  -> daemon core


Relay:

Browser
  |-- workspace websocket
  v
relay dumb pipe
  |-- opaque binary tunnel
  v
daemon relay adapter
  -> daemon client controller
  -> daemon core
```

direct 与 relay 最终都进入同一个 daemon client controller。relay adapter 只把 relay 转来的 opaque stream 变成 controller 能处理的连接事件。

## 4. Segment 格式

当前实现复用 `ProtocolPacket` 作为 segment 层：

```text
request       普通 unary RPC，例如 session.list / daemon.status / files / git
response      request 或 stream_open 的结果
event         daemon 主动事件，例如 session.activity / session.files / session.git
stream_open   打开一个有序 stream，例如 terminal.attach / terminal.create
stream_chunk  stream 上的有序数据，例如 stdout batch / stdin bytes
stream_end    daemon 正常结束 stream
cancel        browser 取消 stream，例如切换 session
error         request 或 stream 级错误
```

外层 WebSocket 可以是 binary frame。E2EE 后的 `ProtocolPacket` 使用二进制编码，terminal bytes 不再为了 JSON 可读性额外 base64 化；只有兼容路径需要 JSON fallback。

## 5. Terminal Stream

terminal 只需要下面几类语义：

```text
browser -> daemon:
  stream_open terminal.attach(session_id, last_terminal_seq?)
  stream_chunk stdin(bytes)
  request/session.resize 或 terminal resize segment
  cancel(stream_id)

daemon -> browser:
  response terminal.attach(...)
  stream_chunk snapshot(base_seq, bytes)
  stream_chunk output(seq, bytes)
  stream_chunk resize/exit frame
```

约束：

- 一个 terminal stream 绑定一个 session。
- 切换 session 只取消旧 terminal stream，不关闭 workspace WebSocket。
- `stdin`、`stdout`、`resize` 必须保持相对顺序；它们都走同一条可靠 WebSocket。
- PTY exit 后 daemon 删除 session，UI 通过正常 session list / event 感知。
- 多个 browser 可以同时打开同一 session，各自拥有独立 workspace WebSocket 和 terminal stream。

## 6. 非 Terminal RPC

非 terminal 操作继续使用 request/response 或 event：

```text
session.list
session.rename
session.reorder
session.close
session.files
session.git
session.search
daemon.status
daemon.clients
daemon.client_forget
ping
```

这些 RPC 与 terminal stream 共用同一条 WebSocket，但不共享 terminal stream 的 `stream_id`。大输出不能让 daemon 等待应用层 ACK；浏览器端消费慢时只能形成正常传输排队或触发连接级关闭，不能卡住 PTY session。

## 7. Snapshot / Tail 事务

terminal attach 时，daemon 必须按下面顺序发送：

1. `stream_open terminal.attach` 被认证连接接受。
2. 从 daemon mirror 读取当前 session snapshot。
3. 发送 snapshot，browser 清空旧 renderer 并重建画面。
4. 从 snapshot 对应的 `base_seq` 开始发送 tail / live stdout。

如果 snapshot 生成期间 PTY 有新输出，daemon 先写入 mirror，再把这些输出排入该 stream 的 live queue。snapshot 与 tail 的边界属于同一个 `stream_id`。

## 8. Supervisor / Daemon Mirror

supervisor 是 PTY 的原始权威源。daemon mirror 是 browser attach 时生成 snapshot 的近端缓存。

supervisor 和 daemon mirror 都应表达：

1. 普通屏幕 snapshot。
2. 替代屏幕 snapshot。
3. 当前屏幕模式。
4. 光标、保存光标、SGR、VT mode、scroll region、wrap / origin / insert 等可恢复状态。
5. raw output seq / tail cursor。

daemon 收到 supervisor raw bytes 后，顺序必须是：

```text
raw bytes -> daemon mirror emulator -> session room live stream -> workspace websocket terminal streams
```

这样新 browser attach 不需要回 supervisor 读取缓存，也能拿到与 daemon 当前输出完全一致的 snapshot。

## 9. Relay 原则

relay 必须保持 dumb pipe。

允许：

- 接收 browser websocket。
- 接收 daemon 到 relay 的长期连接。
- 按 `server_id` 路由。
- 转发 binary frame。
- 记录连接级别日志和吞吐统计。
- 在连接断开时释放连接记录。

禁止：

- 解密业务数据。
- 解析 session 内容。
- 判断 terminal / 非 terminal 的业务含义。
- 维护 terminal ACK、credit、snapshot、stale session。
- 代替 daemon 做控制权判断。

relay 看到的应该只是“某个 browser 连接”和“某个 daemon 连接”之间的 opaque bytes。

## 10. 背压模型

terminal 输出不使用 render ACK / credit。

新的背压规则：

1. daemon 对每个 workspace WebSocket 使用 bounded queue。
2. queue 满或写入超时，daemon 关闭该慢 workspace WebSocket。
3. 关闭 workspace WebSocket 不影响 PTY session。
4. 切换 session 时使用 `cancel(stream_id)` 停止旧 terminal stream。
5. 大输出按字节批量 flush，目标是 4 KiB 到 16 KiB 或约 10 ms flush。
6. 不按行、不按 terminal frame 数量限速。

## 11. 失败处理

1. browser workspace WebSocket 断开：daemon 清理该 browser 连接上的 streams；session 继续运行。
2. browser 切后台：如果 WebSocket 被系统或网络断开，前台恢复时重建 workspace WebSocket 并重新 attach。
3. relay 断开：daemon 到 relay 的通道离线；session 仍在 daemon/supervisor 中存活。
4. daemon / supervisor 重连：按 supervisor/daemon mirror 同步机制恢复，不通过 relay 兜底业务逻辑。
5. PTY exit：daemon 删除 session，session list 反映删除结果。

## 12. 必测场景

必须覆盖：

1. direct 模式快速切换多个大输出 session。
2. relay 模式快速切换多个大输出 session。
3. relay 下两个客户端同时打开同一持续大输出 session。
4. 两个不同分辨率客户端同时 attach 同一 session，并轮流输入和 resize。
5. 100 ms 双向延迟下，relay 仍能 attach、输入、resize、切换并在秒级恢复。
6. browser hidden / visible 或刷新后，不会保留半开 terminal stream 状态。
7. terminal 输出不会因为缺少 render ACK / credit 停住。
8. 非 terminal RPC 不会新建第二条 browser-daemon WebSocket。
9. relay 日志中不出现业务级 terminal ACK / credit / stale session 处理。
10. supervisor normal / alternate screen snapshot 能正确恢复。

## 13. 不变式

1. relay 不可访问明文。
2. 未 attach 的 terminal stream 不能操作 PTY stdin/stdout。
3. session 不因 browser 断开而终止。
4. terminal stream 的 snapshot 和 stdout 必须属于同一 `stream_id`。
5. 非 terminal RPC 不依赖 terminal stream 生命周期。
6. terminal 输出不等待 browser render ACK。
7. daemon mirror 与 supervisor 可恢复状态保持一致。

## 14. 结论

新模型把 browser-daemon/browser-relay-daemon 都抽象成同一条可靠加密流。terminal 与非 terminal 的区别只存在于 E2EE 内层 segment，而不是 WebSocket 数量或 relay 业务逻辑。这个结构保留 TCP/WebSocket 的原生可靠性，降低 relay 耦合，同时让 session 切换、snapshot/tail、文件/Git/状态和多客户端 attach 的边界保持一致。
