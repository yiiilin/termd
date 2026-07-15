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

- 预编译安装当前支持 **Linux x86_64（amd64）+ systemd**。
- 主机需要 `sudo`、`curl` 或 `wget`、`python3`、`tar`、`sha256sum` 和常见账户管理工具。
- Linux arm64 会自动回退到源码编译，需要 Git、Rust 1.85+ 和 C/C++ build tools；使用 `--web` 还需要 Node.js 22 与 npm。详见[完整安装指南](docs/installation.md#linux-arm64)。
- 下列 `--user "$(id -un)"` 命令应从普通登录用户的 shell 执行，不要先切换到 root shell。

### 本机安装

以下命令安装最新 release、启用内嵌 Web，并让新 session 使用当前用户的 HOME 和 login shell：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --web --user "$(id -un)"
```

需要 HTTP/SOCKS 代理时，先按[代理安装说明](docs/installation.md#通过代理安装)导出代理变量并显式传给 sudo。

截至 0.8.2，`latest` release 仍以 `install-*.sh` 为安装入口，不提供稳定名称的 raw binary。当前源码构建出的二进制和包含本改动的下一 release 才支持 `<binary> install|uninstall`；详见[完整安装指南](docs/installation.md#下一-release-与源码构建)。

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

SSH tunnel 示例中的 `alice@terminal-host` 必须替换成真实 SSH 目标，不能原样执行：

```bash
ssh -N -L 8765:127.0.0.1:8765 alice@terminal-host
```

隧道建立后，本机浏览器仍打开 <http://127.0.0.1:8765>。局域网和公网的完整命令、暴露边界与验证步骤见[安装与升级指南](docs/installation.md)；Nginx 和 Docker Compose 参考见[公网部署方案](docs/deployment.md)。

## 升级

重复执行对应的 `latest` 安装命令即可升级。installer 会保留 `/etc/termd/termd.env`、daemon identity、已配对设备和 `/var/lib/termd`。supervisor compatibility 未变化时，现有 session supervisor 不会被清空；确有不兼容变化时，installer 会先明确提示 session 影响并要求确认。

公网环境固定先升级 `termrelay`，验证 relay health，再升级 `termd`。不要在未备份状态或未读提示时使用 `--purge`。完整升级、回滚前检查和日志命令见[安装与升级指南](docs/installation.md#升级)。

### 历史兼容性说明

0.7.0 曾把 workspace 切换为双 WebSocket，并把 supervisor compatibility 切换为 `2026-07-12-dual-ws`。只有从 0.6.x 或更早版本跨越该边界时，旧 supervisor session 不能保留。当前 release 之间是否兼容以 installer 的实际检查和确认提示为准，不要套用这条历史说明清理新版本 session。

## 卸载

默认卸载只删除程序和 systemd 配置，保留本地状态：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --uninstall
```

删除 identity、配对设备和所有 session 的 `--purge` 操作不可恢复，执行前请阅读[完整卸载说明](docs/installation.md#卸载)。

## License

MIT. See [LICENSE](LICENSE) and [Third-Party Notices](THIRD_PARTY_NOTICES.md).
