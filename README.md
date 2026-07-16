<div align="center">
  <img src="termui/frontend/public/icons/termd.svg" width="96" height="96" alt="Termd icon">
  <h1>Termd</h1>
  <p><strong>个人使用的可信 relay 持久终端。</strong></p>
</div>

Termd 让一台机器上的 shell session 由独立 supervisor 持久托管，并通过浏览器长期存在。客户端断开后可以重新 attach，多个已配对设备默认可以同时操作同一个终端。

项目定位是个人使用：单用户、设备级信任、轻量 relay，不做企业权限平台。

## 功能

- supervisor 托管持久 PTY，daemon 重启或浏览器断开不会自动关闭 session。
- 内嵌 Web UI，包含终端、session、文件和 daemon 管理。
- 短期 pairing invite、持久 device certificate 和 challenge-response 登录。
- 一条 metadata WebSocket 加一条 terminal WebSocket；HTTP control 接口使用 JSON。
- trusted relay 负责公网 admission 和路由，session/PTY 状态仍只保存在 daemon。

## 新用户快速开始

### 系统要求

- 预编译安装支持 **Linux x86_64（amd64）和 arm64（aarch64）+ systemd**。
- 主机需要 `sudo`、`curl`、`python3`、systemd 和常见账户管理工具。
- 下列 `--user "$(id -un)"` 命令应从普通登录用户的 shell 执行，不要先切换到 root shell。

### 本机安装

以下命令安装最新 release、启用内嵌 Web，并让新 session 使用当前用户的 HOME 和 login shell：

```bash
case "$(uname -m)" in
  x86_64|amd64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
asset="termd-linux-${arch}"
curl --proto '=https' --tlsv1.2 -fL \
  "https://github.com/yiiilin/termd/releases/latest/download/${asset}" -o "$asset"
chmod 0755 "$asset"
"./$asset" --version
"./$asset" install --dry-run --web --user "$(id -un)"
sudo "./$asset" install --web --user "$(id -un)"
rm -f "$asset"
```

`--dry-run` 只显示非敏感安装计划。正式 `install` 会再次显示计划并交互确认。首次 bootstrap 下载依赖 GitHub HTTPS；安装后的 `upgrade` 会额外校验 GitHub release asset 提供的 SHA-256 digest。代理、relay 和 CLI 安装见[完整安装指南](docs/installation.md)。

安装后立即验证：

```bash
termd --version
sudo systemctl is-active termd
curl -fsS http://127.0.0.1:8765/healthz | python3 -m json.tool
```

三个命令应分别显示版本号、`active` 和包含 `"status": "ok"` 的 JSON。然后在同一台机器打开 <http://127.0.0.1:8765>。

### 首次配对

安装器会打印一段短期、一次性的 `termd-pair:v2:...` 邀请码。在 Web 配对页粘贴或扫描它。邀请码过期或终端输出已经关闭时，重新签发：

```bash
termd pair --qr
```

邀请码是敏感凭据，不要放入命令参数、URL、聊天记录或日志。

## 选择访问方式

| 场景 | daemon 监听 | 建议 |
| --- | --- | --- |
| 同机浏览器 | `127.0.0.1:8765` | 默认且最简单 |
| 另一台机器临时访问 | `127.0.0.1:8765` | 使用 SSH tunnel，不必暴露端口 |
| 可信 LAN/VPN | `0.0.0.0:8765` | 明文 HTTP，仅允许受控网络并配置防火墙 |
| 公网访问 | `127.0.0.1:8765` | 使用独立主机上的 trusted relay + TLS 反向代理 |

### 公网 relay 快速安装

在 relay 主机运行 `sudo termrelay install --web --listen 127.0.0.1:8080`。安装器会创建或保留 daemon registry 和 setup token，并在完成后直接打印当前 setup token，标记为敏感值。配置好 relay 的 TLS 域名后，在 daemon 主机运行：

```bash
./termd-linux-${arch} install --dry-run --web --user "$(id -un)" \
  --relay wss://relay.example.com
sudo ./termd-linux-${arch} install --web --user "$(id -un)" \
  --relay wss://relay.example.com
```

dry-run 只说明正式安装会询问 token，不读取输入；正式安装会隐藏输入内容。非交互安装必须使用 `--relay-token <TOKEN>` 或更安全的 `--relay-setup-token-file <PATH>`。安装器会按本机 `server_id` 验证 relay control 连接，明确打印 `SUCCESS`/`FAILED`，然后运行真实的 `termd pair --qr` 流程输出二维码和邀请码。任一 post-install 检查失败时命令非零退出，并说明本地 service 已安装及精确重试命令。完整两主机步骤及安全说明见[安装指南](docs/installation.md#公网-trusted-relay两主机流程)。

SSH tunnel 示例中的 `alice@terminal-host` 必须替换成真实 SSH 目标，不能原样执行：

```bash
ssh -N -L 8765:127.0.0.1:8765 alice@terminal-host
```

隧道建立后，本机浏览器仍打开 <http://127.0.0.1:8765>。局域网和公网的完整命令、暴露边界与验证步骤见[安装与升级指南](docs/installation.md)；Nginx 和 Docker Compose 参考见[公网部署方案](docs/deployment.md)。

## 升级

三个程序都能自行探测、下载并校验最新版本。公网环境固定先升级 relay，确认 health 后再升级 daemon：

```bash
sudo termrelay upgrade
sudo systemctl is-active termrelay
sudo termd upgrade
sudo termctl upgrade
```

升级器严格比较 semver，下载当前架构的裸二进制并校验 GitHub asset 的 SHA-256 digest，再调用新程序自带的 installer 原子替换、重启并检查服务。配置、daemon identity、已配对设备和 `/var/lib/termd` 会保留。supervisor compatibility 变化时，`termd` 会额外警告现有 session 将丢失并要求独立确认；`--yes` 不会代替该确认，自动化必须明确添加 `--allow-session-loss`。

不要在未备份状态或未读提示时使用 `--purge`。完整升级、回滚前检查和日志命令见[安装与升级指南](docs/installation.md#升级)。

### 历史兼容性说明

0.7.0 曾把 workspace 切换为双 WebSocket，并把 supervisor compatibility 切换为 `2026-07-12-dual-ws`。只有从 0.6.x 或更早版本跨越该边界时，旧 supervisor session 不能保留。当前 release 之间是否兼容以 installer 的实际检查和确认提示为准，不要套用这条历史说明清理新版本 session。

## 卸载

默认卸载只删除程序和 systemd 配置，保留本地状态：

```bash
sudo termd uninstall
```

删除 identity、配对设备和所有 session 的 `--purge` 操作不可恢复，执行前请阅读[完整卸载说明](docs/installation.md#卸载)。

## License

MIT. See [LICENSE](LICENSE) and [Third-Party Notices](THIRD_PARTY_NOTICES.md).
