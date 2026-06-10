# Supervisor Terminal Attach Protocol

## 目标

attach 成功后，`client -> relay -> termd -> supervisor` 这条链路里的终端数据只允许以 opaque frame 形式转发：

- `supervisor` 定义终端消息
- `supervisor` 发送 heartbeat，并裁决 attach 超时
- `termd` / `relay` 不解析终端业务字段
- `client` 直接编解码 `supervisor` 终端消息

## 物理承载

- 外层仍使用 packet stream：`terminal.create` / `terminal.attach`
- stream 内统一使用 `attach_frame` payload
- `attach_frame.data_base64` / binary bytes 内部承载一整帧 length-prefixed JSON supervisor 消息

## attach bootstrap

`termd` 在 stream-open 阶段完成：

1. session/device 权限校验
2. 创建 watched attachment
3. 将 `last_terminal_seq` 作为 bootstrap 参数交给 `supervisor`

bootstrap 完成后，后续终端输入/输出都走 opaque attach frame。

## Supervisor -> Client 消息

### `attach_sync`

首次 attach 成功后发送，包含：

- `session_id`
- `base_seq`
- `snapshot`
- `frames`

其中 `snapshot` 提供当前完整屏幕，`frames` 提供 `base_seq` 之后的 tail。

### `terminal_frame`

增量终端帧，直接复用现有语义：

- `snapshot`
- `output`
- `resize`
- `exit`

### `heartbeat_ping`

`supervisor` 定时发送。client 必须回 `heartbeat_pong`，否则当前 attach 被关闭。

字段：

- `nonce`
- `timeout_ms`

### `close`

仅关闭当前 attach，不关闭 session。

字段：

- `reason`
- `message`

建议 reason：

- `heartbeat_timeout`
- `server_shutdown`
- `attach_replaced`
- `protocol_error`

## Client -> Supervisor 消息

### `input`

- `data_base64`

### `resize`

- `size`

### `heartbeat_pong`

- `nonce`

## 不变量

- attach 超时只影响当前 attach
- `session_data` / `terminal_frame` 不再作为 packet terminal stream 的业务载体
- `attach_frame` 只表达“opaque supervisor frame”，不表达 daemon 业务语义
