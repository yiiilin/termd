# AGENTS.md

本文件定义 termd 项目的开发约定、架构边界和实现原则。

---

# 1. 项目目标

构建一个：

> **端到端加密的持久终端系统（terminal-as-a-service）**

核心能力：

* session 持久化
* SSH 会话复用
* relay 网络支持
* 设备级认证（pairing）
* 多客户端 attach

---

# 2. 核心设计原则

## 2.1 简单优先（MVP）

禁止提前复杂化：

* ❌ 不实现 RBAC
* ❌ 不实现多用户系统
* ❌ 不实现复杂权限
* ❌ 不实现企业功能

允许：

* ✅ 单用户
* ✅ 设备级信任
* ✅ 单控制者

---

## 2.2 信任模型

```text
daemon public key = trust anchor
device key = identity
pairing = trust establishment
relay = untrusted
```

---

## 2.3 权限模型（固定）

```text
无权限系统
仅：

- controller（唯一）
- viewer（其他设备）
```

规则：

* 第一个 attach = controller
* 其他设备 = viewer
* 任意设备可 steal control

---

## 2.4 relay 原则

relay **必须是 dumb pipe**：

禁止：

* ❌ 解密数据
* ❌ 解析 session 内容
* ❌ 执行权限判断

只允许：

* 转发 WebSocket 数据
* 路由 server_id
* 管理连接

---

# 3. 通信协议约定

## 3.1 传输层

统一：

```text
HTTP + WebSocket
ws / wss
```

---

## 3.2 消息格式

```json
{
  "type": "string",
  "payload": {}
}
```

---

## 3.3 必备消息类型

```text
hello
auth
pair_request
pair_accept
session_attach
session_data
session_resize
control_request
control_grant
ping/pong
```

---

## 3.4 数据流约束

* session_data 必须是二进制或 base64
* 不允许混合协议格式
* 所有敏感数据必须在 E2EE 内

---

# 4. 代码结构约定

以下结构描述目标模块边界，不表示目录在当前阶段都已经完全实现。当前已存在的 Rust crate 是
`/proto`、`/termd`、`/termctl` 和 `/termrelay`；`/termui/frontend` 是 Web MVP，`/termui/native` 是 Flutter 架构骨架。

```text
/proto

/termd
  /session
  /pty
  /auth
  /net

/termrelay
  /ws
  /router

/termctl
  /cmd

/termui
  /frontend
```

---

# 5. 状态机要求（必须遵守）

## session

```text
CREATED → RUNNING → CLOSED
```

## connection

```text
INIT → AUTH → ATTACHED → CLOSED
```

## control

```text
NONE → HELD(dev_x) → HELD(dev_y)
```

---

# 6. 安全要求

必须实现：

* pairing token 过期
* device key 验证
* challenge-response auth
* replay protection（timestamp / nonce）

推荐：

* Noise protocol
* X25519

---

# 7. 不变量（Critical Invariants）

必须始终成立：

```text
1. 一个 session 只能有一个 controller
2. 未配对设备不能连接
3. relay 不可访问明文
4. session 不因 client 断开而终止
```

---

# 8. 开发优先级

顺序：

1. termd 内核（PTY + session）
2. pairing + auth
3. WebSocket 协议
4. termctl
5. termrelay
6. termui

---

# 9. 禁止事项

```text
❌ 在 relay 中写业务逻辑
❌ 在 client 中存储 server 私钥
❌ 引入复杂依赖（过早优化）
❌ 把协议写死在 UI 层
```

---

# 10. 未来扩展（暂不实现）

```text
RBAC
组织 / 多用户
session 录像
文件传输
SSO
Kubernetes 集成
```

---

# 11. 一句话原则

```text
保持 termd 像 sshd + tmux 的组合，
而不是一个“复杂平台产品”
```
