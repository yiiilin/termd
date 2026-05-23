# 终端双线架构设计

> 目标：把浏览器侧通信收敛成 `terminal lane` 和 `aux lane` 两条独立通道，并让 direct / relay 走同一套 daemon-side client controller 语义。

## 背景

当前系统已经具备持久 session、E2EE、relay 路由和 PTY supervisor，但历史实现把终端输出、控制 RPC、文件/Git/状态查询混在一起，容易出现以下问题：

1. 大量终端输出把普通 RPC 和输入响应挤在后面。
2. relay / daemon 的重连边界不清，容易留下半开连接或旧 session 残留。
3. snapshot / tail / resize 的顺序不够明确，导致重连时画面恢复和实时 tail 可能错位。

本设计把这些问题拆成两条明确的数据线，并把 relay 限制为纯转发层。

## 设计原则

1. relay 只做路由，不解释业务。
2. 终端相关操作必须在同一条 terminal lane 上保持顺序。
3. 非终端 RPC 不能被大输出阻塞。
4. session 生命周期独立于 browser 连接生命周期。
5. direct 和 relay 的语义必须一致，只允许 transport 不同。

## 术语

- `terminal lane`：承载 `snapshot`、`stdout`、`stdin`、`resize`。
- `aux lane`：承载文件、Git、状态、列表、重命名、关闭等非终端操作。
- `snapshot`：连接恢复时的权威终端快照。
- `tail`：snapshot 之后的连续增量输出。
- `mirror`：daemon 侧为 reconnect 和 attach 维护的终端缓存副本。

## 总体结构

```text
Browser
  ├─ terminal websocket ───────────────┐
  └─ aux websocket ────────────────────┤
                                       v
                                 direct daemon
                                       │
Browser
  ├─ terminal websocket ─ relay ───────┤
  └─ aux websocket ─ relay ────────────┘
                                       v
                              daemon client controller
                                       │
                                 session / PTY supervisor
```

### 选择理由

我们不采用“单 websocket 混合终端和 RPC”方案，原因是：

1. 大 snapshot 和长 tail 会造成 head-of-line blocking。
2. 输入和 resize 需要和终端输出保持严格顺序，但普通 RPC 不需要。
3. relay 侧只要出现发送背压，就不该拖慢 aux RPC 的恢复。

所以终端线和非终端线分离是最稳妥的边界。

## direct / relay 语义

### direct 模式

- Browser 直接连 daemon。
- terminal lane 进入 daemon 的 terminal controller。
- aux lane 进入 daemon 的 control / metadata controller。

### relay 模式

- Browser 先连 relay。
- relay 只做 websocket 路由和连接管理。
- relay 不解密、不解析 session 内容、不做权限判断。
- relay 只把两条 lane 原样转发到 daemon。

### 统一语义

无论 direct 还是 relay，daemon 看到的都是同一组 controller 事件：

- terminal attach / snapshot / stdout / stdin / resize
- aux RPC：files / git / status / list / rename / reorder / close / control_request

## terminal lane 设计

terminal lane 只做终端语义，不混入管理 RPC。

### 入站

- `stdin`：用户键盘输入。
- `resize`：终端尺寸变化。

### 出站

- `snapshot`：重连或首次 attach 时的全量画面。
- `stdout`：PTY 后续增量输出。

### 约束

1. `stdin` 和 `resize` 必须和同一 session 的 `snapshot/stdout` 保持顺序。
2. `close` 不作为 terminal message 单独存在。
3. PTY 退出时直接由 daemon 清理 session。
4. 终端输出按字节批量发送，不按“每一行一条消息”推送。
5. 批量 flush 以字节阈值或短时间窗口为准，避免一字节一字节蹦。

## aux lane 设计

aux lane 只承载非终端 RPC：

- session list
- daemon status
- daemon clients
- files / download / write / delete
- git / diff / action
- rename / reorder / close

### 约束

1. aux lane 不能要求终端 renderer 参与。
2. aux lane 不能等待 terminal tail 结束。
3. aux lane 不得依赖 PTY 输出是否清空。

## supervisor / daemon 缓存模型

### supervisor 侧

supervisor 维护三类状态：

1. 普通屏幕 snapshot
   - 最近 1000 行逻辑内容
   - cursor 位置
   - saved cursor
   - 当前终端模式
   - scrollback 范围
2. 替代屏幕 snapshot
   - 当前可见页
   - cursor 位置
   - 当前终端模式
3. 当前模式
   - normal
   - alternate

supervisor 在读取 PTY 输出时，必须同步更新这些缓存。

### daemon 侧 mirror

daemon 为每个 session 维护和 supervisor 等价的 mirror：

- normal screen mirror
- alternate screen mirror
- current mode
- cursor / saved cursor / modes
- terminal sequence / tail cursor

mirror 的作用有两个：

1. 新 browser attach 时直接出 snapshot。
2. daemon 与 supervisor 连接重建时，能判断是否需要回源 supervisor 重新取权威 snapshot。

### mirror 更新规则

daemon 收到 raw bytes 后必须先喂给本地 mirror emulator，完成状态更新，再进入 browser room live stream。
这样后来的 attach 可以直接拿到最新 snapshot，不需要回读 PTY 原始流。

## snapshot / tail 生命周期

### 首次 attach

1. Browser 建立 terminal lane。
2. Daemon 发送当前权威 snapshot。
3. Browser 清空旧 renderer 并应用 snapshot。
4. Daemon 继续发送 tail。

### reconnect

1. 如果 connection generation 未变，daemon 先发新的 snapshot。
2. Browser 必须先 clear / reset，再重放 snapshot。
3. snapshot 结束后才能继续接 tail。
4. 如果 sequence gap 无法证明连续性，直接回源 supervisor 重新生成 snapshot。

### 断线

- browser 断开只关闭当前 attachment，不终止 session。
- daemon / supervisor 断开只会触发 reconnect 或回源重建，不自动杀 PTY。

## 顺序与背压

### terminal lane

- 终端 lane 的顺序是强约束。
- 输入、resize、stdout、snapshot 的相对顺序不能乱。
- 大 snapshot 不能把 aux lane 一起堵住。

### aux lane

- aux RPC 可以独立重试。
- aux lane 不应因为终端输出 backlog 而延迟数秒。

### relay

- relay 必须把 terminal lane 和 aux lane 的队列分开。
- control frame、disconnect 通知、新 attach 不能排在旧大输出后面。
- relay 只做批量转发，不做内容级拆包或重排。

## 失败处理

1. relay 连接断开：只裁定该路由不可用，不清理 session。
2. daemon / supervisor 断开：terminal lane 重连后重新拿 snapshot。
3. browser hidden / background：暂停主动重连，避免残留半开连接。
4. PTY exit：daemon 直接结束 session。
5. sequence gap：回源 snapshot，不做局部补洞。

## 测试矩阵

必须覆盖：

1. direct 模式下快速切换多个大输出 session。
2. relay 模式下快速切换多个大输出 session。
3. relay 下双客户端同时打开同一 session。
4. 100ms 双向延迟下的 attach / 输入 / resize / 切换恢复。
5. browser hidden / visible 切换时不会堆积半开 websocket。
6. supervisor normal / alternate screen snapshot 能正确还原。
7. daemon mirror 缺失时会回源 supervisor，而不是拼接错误 tail。

## 不变式

1. relay 不可访问明文。
2. 未 attach 的连接不能操作 session。
3. session 不因 browser 断开而终止。
4. terminal lane 顺序优先于 aux lane 的响应速度。
5. snapshot 和 tail 必须属于同一代连接视图。

## 结论

这个架构把终端高频流量和普通管理 RPC 分开，保留 direct / relay 的一致语义，同时把 snapshot / tail / reconnect 的边界固定下来。后续实现只需要围绕这份边界收敛，不要再把业务逻辑塞进 relay。
