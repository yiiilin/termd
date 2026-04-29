# termd

**termd** 是一个面向开发者的持久化远程终端系统。

它允许你在服务器上运行一个常驻 daemon，并通过 CLI / GUI / Web / Mobile 客户端连接、恢复、共享终端会话，即使网络断开也不会丢失 session。

---

## ✨ 特性

* 🔁 **持久会话**：断线不掉 session
* 🔐 **端到端加密（E2EE）**：relay 无法读取终端内容
* 🔗 **直连 + Relay**：支持 NAT / 内网穿透
* 📱 **扫码 / 粘贴配对**：无需账号密码
* 👥 **多端连接**：多个客户端同时查看
* 🎮 **单控制者模型**：避免输入冲突
* 🧠 **SSH 会话复用**：远程 SSH 不中断

---

## 🧱 架构

```text
termui / termctl
        │
        │ WebSocket (ws / wss)
        ▼
    termrelay (optional)
        ▲
        │
      termd
```

组件说明：

* **termd**：运行在服务器上的核心 daemon
* **termctl**：CLI 管理工具
* **termui**：GUI 客户端（Web / Desktop / Mobile）
* **termrelay**：中继服务（可选）

---

## 🚀 快速开始（MVP）

### 1. 启动 daemon

```bash
termd start
```

---

### 2. 生成配对二维码

```bash
termd pair --qr
```

或输出配对密文：

```bash
termd pair
```

---

### 3. 客户端连接

* 使用 **termui** 扫码
* 或粘贴 pairing code

---

### 4. 创建 session

```bash
termctl new
```

或：

```bash
termctl new --cmd "ssh user@host"
```

---

### 5. 连接 session

```bash
termctl attach <session_id>
```

---

## 🔐 安全模型

* daemon public key = 信任根
* pairing = 注册设备
* 每个设备使用独立 key
* relay 不可信（只转发密文）

---

## 🌐 Relay（可选）

启动 relay：

```bash
termrelay --listen :8080
```

支持：

* ws://（开发）
* wss://（生产）
* 可通过 Nginx / Caddy 反代

---

## 📡 通信协议

* HTTP/1.1 + WebSocket
* JSON message envelope
* E2EE（Noise / X25519）

---

## 📦 Monorepo 结构

```text
/termd        # daemon
/termctl      # CLI
/termui       # GUI client
/termrelay    # relay server
/proto        # protocol definition
/docs         # design docs
```

---

## 🎯 MVP 范围

已实现：

* daemon + PTY
* session 持久化
* pairing（扫码 / 粘贴）
* 单控制者模型
* relay 转发

未实现：

* 权限系统
* 多用户
* 审计
* 文件传输

---

## 📜 License

TBD
