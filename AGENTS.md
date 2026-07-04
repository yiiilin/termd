# AGENTS.md

本文件定义 termd 项目的开发约定、架构边界和实现原则。

---

# 1. 项目目标

构建一个：

> **可信 relay 的持久终端系统（terminal-as-a-service）**

核心能力：

* session 持久化
* SSH 会话复用
* relay 网络支持
* 设备级认证（pairing）
* 多客户端 attach

---

# 2. 核心设计原则

## 2.1 简单优先（MVP）

项目定位是个人使用，不做多人平台。

允许：

* ✅ 单用户
* ✅ 设备级信任
* ✅ 单控制者

---

## 2.2 信任模型

```text
daemon public key = identity anchor
device key = identity
pairing = trust establishment
relay = trusted admission/routing layer
```

---

## 2.3 控制权模型（固定）

```text
无账号/平台策略系统
仅：

- operator（所有已 attach 设备）
```

规则：

* 任意已配对设备 attach 后都是 operator
* 多个 operator 默认 shared-control，可同时向同一个 PTY 输入
* `control_request` 仅作为旧命令的 noop 确认路径，不表达夺权

---

## 2.4 relay 原则

relay **是可信 admission 和 routing 层**：

禁止：

* ❌ 持有 session/PTY 业务状态
* ❌ 执行终端控制权判断
* ❌ 保存设备私钥或 daemon 私钥

只允许：

* 校验 relay transport token 和 daemon admission token
* 转发明文 WebSocket/HTTP tunnel 数据
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
* pairing/auth/session 权限必须由 daemon 最终校验
* relay secret 不能进入日志、URL access log 或错误事件

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
1. 只有已 attach 的连接可以操作 session
2. 未配对设备不能连接
3. relay 只做可信 admission/routing，不持有 session/PTY 状态
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

# 10. 发版要求

每次发版必须携带清晰的改动内容：

* tag 或 GitHub Release 说明必须列出本次用户可感知的功能、修复和兼容性变化
* 如果只有 git tag 而没有 GitHub Release，也必须在 tag message 中写明改动摘要
* 改动说明应面向使用者描述行为变化，不只写内部文件名或提交号

---

# 11. 个人使用边界

```text
保持单用户、设备级信任和轻量 relay。
不要把 termd 扩展成多人平台。
```

---

# 12. 一句话原则

```text
保持 termd 像 sshd + tmux 的组合，
而不是一个“复杂平台产品”
```
