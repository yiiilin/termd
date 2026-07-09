<div align="center">
  <img src="termui/frontend/public/icons/termd.svg" width="96" height="96" alt="Termd icon">
  <h1>Termd</h1>
  <p><strong>个人使用的可信 relay 持久终端。</strong></p>
</div>

Termd 让一台机器上的 shell session 由 session supervisor 持久托管，并通过浏览器长期存在：client 断开后可以重新 attach，多个已配对设备默认 shared-control，可以同时操作同一个终端。

项目定位是个人使用：单用户、设备级信任、轻量 relay，不做企业权限平台。

## Features

- supervisor-owned 持久 session：每个 session 由独立 supervisor 托管真实 PTY、terminal journal、attach heartbeat 和超时关闭；daemon 只维护 session catalog、workspace/file/git API 和 attach proxy。
- Web UI：内嵌终端、session 管理、文件面板、daemon 管理和 PWA；终端渲染使用 `xterm.js` 单一路径。
- 多客户端 shared-control：已配对设备都是 operator，可同时 attach 同一个 session。
- 设备级 pairing/auth：短期 pairing token、device key、challenge-response、timestamp/nonce replay protection。
- 明文业务协议：去掉运行时 E2EE 后，pairing/auth/session/file 仍由 `termd` 校验和持有，线上路径更短。
- 可信 Relay：`termrelay` 用 setup token 注册 daemon，并用 daemon registry 做入口控制，一个 relay 可服务多个已注册 daemon。
- Web-first client：Web 是正式交互客户端；`termctl` 保留为配对/调试工具。
- 一键安装：`termd`、`termrelay` 支持 curl/wget；`termd` 和 `termrelay` 支持 systemd。

## 使用方式

Release 由 tag 驱动；固定版本时把 URL 里的 `latest` 换成对应 tag。

### 0.6.0 破坏性升级

0.6.0 把 relay 信任模型从“不可信 relay + 运行时 E2EE”切换为“可信 relay + daemon 注册”。升级时请同步升级 `termd`、`termrelay`、Web UI 和 `termctl`，不要混跑 0.5.x daemon 和 0.6.x relay。

- relay 安装后会生成 setup token；daemon 使用 setup token 首次注册，并自动生成自己的 daemon token。
- trusted relay 不再把旧 `relay_token` query 当作浏览器/termctl 的主要 admission；浏览器和 `termctl` 使用 `termd pair --qr` 生成的短期 pairing invite。
- relay registry 会保存 daemon token hash；请备份 `/var/lib/termrelay/daemon-registry.json` 和 `/etc/termd/termrelay_setup_token`。
- daemon 本地状态和已有 session supervisor 不需要因为 0.6.0 自动清空；如果 relay 入口无法识别旧设备，重新执行一次 `termd pair --qr` 配对即可。

### daemon + Web

个人机器推荐指定运行用户，这样 Web 新建 session 会使用该用户的 HOME 和 login shell：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --web --listen 0.0.0.0:8765 --user "$USER"
```

只本机访问可以去掉 `--listen 0.0.0.0:8765`；只想用默认受限用户可以去掉 `--user "$USER"`。

安装脚本会注册并启动 `termd.service`，然后打印一次性 `termd-pair:v2:<base64url>` 邀请码。邀请码过期或用过后，在 daemon 主机重新签发：

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
TERMD_RELAY_DAEMON_TOKEN_FILE=/etc/termd/termd_daemon_token
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
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash -s -- --web --listen 0.0.0.0:8080
```

安装脚本会创建 `/var/lib/termrelay/daemon-registry.json` 和 `/etc/termd/termrelay_setup_token`，并打印 setup token 文件路径。setup token 只给 daemon 注册使用，不放进浏览器 URL。

让 daemon 连接 relay：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash -s -- --relay wss://relay.example --relay-setup-token-file /etc/termd/termrelay_setup_token
```

`termd` 安装脚本会自动生成 `/etc/termd/termd_daemon_token`，用 relay setup token 把 `server_id -> daemon token hash` 注册到 relay。浏览器和 `termctl` 只需要短期 pairing invite；旧 `relay_token` query 不再作为 trusted relay 的浏览器 admission。

同一份 `termd pair --qr` 邀请码可用于 daemon Web 和 relay Web。relay 做 admission 和路由，pairing/auth/session 权限仍由 daemon 最终校验。Docker Compose 部署见 [docs/deployment.md](docs/deployment.md)。

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
