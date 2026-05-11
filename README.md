# termd

**termd** 是一个面向个人开发者的端到端加密持久终端系统。当前 MVP 重点是：daemon 持有 PTY session，受信任设备可以重新 attach，同一个 session 不会因为某个 client 断开而终止。

项目定位是个人使用：单个 daemon、设备级信任、默认 shared-control。多个已配对设备 attach 到同一个 session 后都是 operator，可以同时操作同一个终端。当前仓库仍处于 MVP 阶段，完整 Native client 还没有交付。

## Features

- 持久 PTY session：session 生命周期独立于单个 client 连接，client 断开后可以重新 attach。
- 多客户端 shared-control：多个已配对设备可以同时进入同一个 session 并操作终端。
- 设备级 pairing/auth：短期 pairing token、device key 验证、challenge-response 和 timestamp/nonce replay protection。
- E2EE 通信：使用 X25519 + HKDF + ChaCha20Poly1305 封装业务 frame，relay 不解密、不解析业务内容。
- Web UI 和 CLI：内嵌 Web 终端支持 session、文件面板和 daemon 管理；`termctl` 支持 `pair/new/attach/control/resize/list`。
- relay 网络：`termrelay` 是 dumb pipe，按 daemon 的公开 `server_id` 路由转发 WebSocket；每个 `termd` 只主动连接一个 relay，一个 `termrelay` 可以转发多个 daemon，Web 默认使用当前页面地址连接。
- 一键安装与发布：`termctl`、`termd`、`termrelay` 支持 curl/wget 安装；`termd` 和 `termrelay` 支持 systemd，`termrelay` 另有 docker-compose 部署方式。

非目标：把 termd 做成多人平台或企业权限系统。当前设计只面向个人使用和设备级信任。

## 使用方式

release 资产由 tag 驱动。下面是一键使用入口；如果你要固定版本，把 `latest` 换成对应 tag。

### termd + Web

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web
```

如果需要让局域网或公网反向代理访问 daemon，可以直接在安装时设置监听地址：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --listen 0.0.0.0:8765
```

安装脚本会注册并启动 `termd.service`，随后在当前终端打印一个短期一次性 pairing token、Web 邀请码和 `termctl pair` 示例。token 不会写入配置文件；过期或用过后可在 daemon 主机上运行 `termd pair` 重新签发。

#### 运行用户、默认 shell 和工作目录

`termd` 默认创建并使用受限的 `termd` system user。这个用户的 login shell 是 `/usr/sbin/nologin`，安装脚本会把这种不可交互 shell 回退成 `/bin/sh` 写入 systemd 的 `SHELL` 环境变量；因此默认安装时，Web 新建 session 可能会进入 `/bin/sh`，默认工作目录是 `/var/lib/termd`。

个人机器上如果希望 Web 新建的终端像 SSH 一样直接进入自己的账号、自己的 home 目录和自己的 login shell，推荐安装时指定 `--user <USER>`。该用户必须已存在，session 默认工作目录和默认 shell 会使用该用户的 home/login shell：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --user alice
```

`SHELL` 只影响之后新建的 session，已经运行中的旧 session 不会自动切换 shell。修改 systemd unit 后需要执行 `sudo systemctl daemon-reload && sudo systemctl restart termd`。

### 可选：termctl

`termctl` 是 CLI client。只使用内嵌 Web 时不必安装。

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

### 可选：termrelay

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --listen 0.0.0.0:8080 --web
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --listen 0.0.0.0:8080 --web
```

`termrelay` 是不可信转发层，只按 daemon 内部路由标识转发 WebSocket frame，不解密、不解析业务内容。它也提供 [docker-compose 部署方式](docs/deployment.md#termrelay-docker-compose)。

### relay Web

`termd pair --qr` 现在输出的是单行邀请码，形如 `termd-pair:v1:<base64url>`。它是对 pairing JSON 的 URL-safe 包装，便于复制粘贴；它不是长期密钥，仍会随 pairing token 过期。

同一份邀请码不携带 direct/relay 地址，只携带 daemon 标识和短期 pairing token。打开 daemon Web 时默认连当前 daemon 页面地址；打开 relay Web 时默认连当前 relay 页面地址；需要切换到其他地址时，在 Web 的高级地址设置里手动填写。relay 本身不读取 pairing token，也不做身份判断，只在 `/ws` 上按 `route_hello` 里的 `server_id` 转发密文，pairing token 仍由 daemon 验证。

新安装 `termd` 时直接加入 relay：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --relay wss://relay.example --relay-auth-token relay-secret
```

安装脚本会启动 `termd.service`，并打印一份 `web invite code`。这份邀请码在 daemon Web 和 relay Web 都可用；用户不需要查看或拼接 daemon 路由 ID。

已安装的 `termd` 后续加入 relay，可以重跑安装脚本写入 systemd env 并重启，脚本会重新打印可用于 relay Web 的邀请码：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --relay wss://relay.example --relay-auth-token relay-secret
```

如果 relay 没有启用 auth token，安装命令不传 `--relay-auth-token` 即可。

### 卸载

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash -s -- --uninstall
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash -s -- --uninstall
```

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --uninstall
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --uninstall
```

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --uninstall
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --uninstall
```

`termd` 和 `termrelay` 默认卸载只删除二进制、wrapper、systemd unit 和 `/etc/termd/*.env`，会保留 `/var/lib/termd` / `/var/lib/termrelay`。如需连本地状态目录和 system user 一并删除，追加 `--purge`：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --uninstall --purge
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --uninstall --purge
```

## 安全模型

`daemon public key` 是信任根，`device key` 是设备身份，pairing 是建立信任的过程。`server_id` 只用于 relay 路由。relay 始终按不可信 dumb pipe 处理，只转发密文 frame，不接触终端明文、文件内容或 session 权限判断。开发验证矩阵见 [docs/qa.md](docs/qa.md)，公网部署细节见 [docs/deployment.md](docs/deployment.md)。

## License

MIT. See [LICENSE](LICENSE).
