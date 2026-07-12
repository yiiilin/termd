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
- 设备级 pairing/auth：短期 pair ticket 建立信任，持久 device certificate 绑定设备 key，后续 challenge-response 换取 5 分钟 access token。
- 双 WebSocket workspace：每个工作台固定一条 metadata socket 和一条 terminal socket；session、client、状态和 CWD 由 metadata 推送，终端 snapshot/PTY stream 独占 terminal socket。
- 明文业务协议：去掉运行时 E2EE 后，pairing/auth/session/file 仍由 `termd` 校验和持有，线上路径更短。
- 可信 Relay：`termrelay` 用 setup token 注册 daemon token 与 Ed25519 public key，随后离线校验 daemon 签名的凭据并路由明文流量；不保存 pair ticket、device certificate、access token 或 PTY 状态。
- Web-first client：Web 是正式交互客户端；`termctl` 保留为配对/调试工具。
- 一键安装：`termd`、`termrelay` 支持 curl/wget；`termd` 和 `termrelay` 支持 systemd。

## 使用方式

Release 由 tag 驱动；固定版本时把 URL 里的 `latest` 换成对应 tag。

### 0.7.0 破坏性升级

0.7.0 把 Web workspace 收敛为双 WebSocket + JSON HTTP control，并把 supervisor 兼容版本切换到 `2026-07-12-dual-ws`。daemon、relay、Web UI 和 CLI 必须同步升级；0.6.x client/daemon 不与 0.7.0 协议混跑。

升级顺序固定为 relay 在前、termd 在后。先更新公网 `termrelay` 并确认 `/healthz`；再把 relay setup token 通过安全通道放到 daemon 主机的 root-only 临时文件，用它让新版 termd 重新注册 daemon public key。installer 检测到 supervisor compatibility 变化时会明确提示旧 session 将被清理，只有确认后才继续：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash
sudo install -m 0600 /secure/source/termrelay_setup_token /run/termd-relay-setup-token
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --web --relay wss://relay.example \
      --relay-setup-token-file /run/termd-relay-setup-token
sudo rm -f /run/termd-relay-setup-token
```

升级不会删除 daemon identity、已配对设备或普通配置；既有设备通过受限 migration endpoint 换取 device certificate。旧 session supervisor 与新的 terminal attach 协议不兼容，因此不能保留。完整协议见 [docs/protocols/v0.7-workspace.md](docs/protocols/v0.7-workspace.md)，公网升级步骤见 [docs/deployment.md](docs/deployment.md)。

### 0.6.0 历史升级说明

0.6.0 把 relay 信任模型从“不可信 relay + 运行时 E2EE”切换为“可信 relay + daemon 注册”。升级时请同步升级 `termd`、`termrelay`、Web UI 和 `termctl`，不要混跑 0.5.x daemon 和 0.6.x relay。

- relay 安装后会生成 setup token；daemon 使用 setup token 首次注册，并自动生成自己的 daemon token。
- 0.6 开始迁移旧 `relay_token` query；该迁移兼容已在 0.7 完全结束，0.7 relay 不接受任何 query credential。浏览器和 `termctl` 使用 `termd pair --qr` 生成的短期 pairing invite。
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

把 setup token 通过安全通道放到 daemon 主机的 root-only 临时文件，再让 daemon 连接 relay：

```bash
sudo install -m 0600 /secure/source/termrelay_setup_token /run/termd-relay-setup-token
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --relay wss://relay.example \
      --relay-setup-token-file /run/termd-relay-setup-token
sudo rm -f /run/termd-relay-setup-token
```

`/secure/source/termrelay_setup_token` 表示已经安全传到 daemon 主机的文件，不是公开下载地址。`termd` 安装脚本会自动生成 `/etc/termd/termd_daemon_token`，用 relay setup token 把 `server_id -> (daemon token hash, daemon public key)` 注册到 relay。setup token 必须通过 root-only 文件提供，不放进命令参数、URL 或日志；relay 不接收 pair ticket、device certificate 或 access token 的同步注册。

同一份 `termd pair --qr` 邀请码可用于 daemon Web 和 relay Web。首次配对通过 `POST /api/auth/pair` 获取持久 device certificate；后续由设备私钥完成 challenge-response，换取 5 分钟 access token，并提前 60 秒刷新。每个 Web workspace 固定打开 `/ws/metadata` 和 `/ws/terminal` 两条连接；其余 control API 使用 JSON，只有上传 chunk request 和下载 byte body 是原始字节。relay 做可信 admission 和路由，最终 pairing/auth/session 权限仍由 daemon 校验。Docker Compose 部署见 [docs/deployment.md](docs/deployment.md)。

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

See [Third-Party Notices](THIRD_PARTY_NOTICES.md) for bundled components.
