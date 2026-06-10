<div align="center">
  <img src="termui/frontend/public/icons/termd.svg" width="96" height="96" alt="Termd icon">
  <h1>Termd</h1>
  <p><strong>个人使用的端到端加密持久终端。</strong></p>
</div>

Termd 让一台机器上的 shell session 由 session supervisor 持久托管，并通过浏览器长期存在：client 断开后可以重新 attach，多个已配对设备默认 shared-control，可以同时操作同一个终端。

项目定位是个人使用：单用户、设备级信任、轻量 relay，不做企业权限平台。

## Features

- supervisor-owned 持久 session：每个 session 由独立 supervisor 托管真实 PTY、terminal journal、attach heartbeat 和超时关闭；daemon 只维护 session catalog、workspace/file/git API 和 attach proxy。
- Web UI：内嵌终端、session 管理、文件面板、daemon 管理和 PWA；终端渲染只使用 `ghostty-web`。
- 多客户端 shared-control：已配对设备都是 operator，可同时 attach 同一个 session。
- 设备级 pairing/auth：短期 pairing token、device key、challenge-response、timestamp/nonce replay protection。
- E2EE 通信：业务 frame 使用 X25519 + HKDF + ChaCha20Poly1305；relay 只转发密文。
- Relay：一个 `termrelay` 可转发多个 daemon，每个 `termd` 主动连接一个 relay。
- Web-first client：Web 是正式交互客户端；`termctl` 保留为配对/调试工具。
- 一键安装：`termd`、`termrelay` 支持 curl/wget；`termd` 和 `termrelay` 支持 systemd。

## 使用方式

Release 由 tag 驱动；固定版本时把 URL 里的 `latest` 换成对应 tag。

### daemon + Web

个人机器推荐指定运行用户，这样 Web 新建 session 会使用该用户的 HOME 和 login shell：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --listen 0.0.0.0:8765 --user "$USER"
```

只本机访问可以去掉 `--listen 0.0.0.0:8765`；只想用默认受限用户可以去掉 `--user "$USER"`。

安装脚本会注册并启动 `termd.service`，然后打印一次性 `termd-pair:v1:<base64url>` 邀请码。邀请码过期或用过后，在 daemon 主机重新签发：

```bash
termd pair --qr
```

常用配置集中在 `/etc/termd/termd.env`，修改后执行 `sudo systemctl restart termd`：

```dotenv
HOME=/home/alice
SHELL=/bin/bash
TERMD_LISTEN=0.0.0.0:8765
TERMD_WEB_ENABLED=1
TERMD_RELAY_URLS=wss://relay.example
TERMD_RELAY_AUTH_TOKEN=relay-secret
```

当前 daemon identity、SQLite 状态库和 supervisor runtime 元数据固定在 `/var/lib/termd`，不随 `--user` 改变。

### CLI / debug

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

`termctl` 不是正式交互 attach 客户端；它只保留配对、诊断和后续调试入口。

### Relay

部署 relay：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --web --listen 0.0.0.0:8080 --auth-token relay-secret
```

让 daemon 连接 relay：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --relay wss://relay.example --relay-auth-token relay-secret
```

同一份 `termd pair --qr` 邀请码可用于 daemon Web 和 relay Web。relay 不读取 pairing token，只按 `server_id` 路由密文 WebSocket frame。Docker Compose 部署见 [docs/deployment.md](docs/deployment.md)。

没有 curl 时，把上面的 `curl -fsSL URL | sudo bash -s -- ...` 换成 `wget -qO- URL | sudo bash -s -- ...`。

### Uninstall

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --uninstall
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --uninstall
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash -s -- --uninstall
```

默认保留 `/var/lib/termd` / `/var/lib/termrelay`。连本地状态和 system user 一起删除：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --uninstall --purge
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --uninstall --purge
```

## License

MIT. See [LICENSE](LICENSE).
