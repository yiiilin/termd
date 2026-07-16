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

## 安装

预编译安装支持 **Linux x86_64（amd64）或 arm64（aarch64）+ systemd**。主机需要 systemd、`sudo`、`curl` 以及发行版自带的基础账户和文件工具；不需要 Python、jq 或 sqlite3。下面每个代码块都是一条 shell compound command，可以整段复制后只按一次回车；临时下载文件会自动清理。

默认按公网使用方式安装：先在公网主机安装 relay，再在保存 shell session 的主机安装 termd，最后按需安装 termctl。仅在 termd 主机本机使用时，可以跳过第 1 步，并在 termd 向导中选择本机直连。

### 1. 安装 termrelay

在 relay 主机执行：

```bash
(set -eu; case "$(uname -m)" in x86_64|amd64) arch=amd64 ;; aarch64|arm64) arch=arm64 ;; *) echo "仅支持 Linux amd64/arm64" >&2; exit 1 ;; esac; asset="termrelay-linux-${arch}"; tmp="$(mktemp)"; trap 'rm -f -- "$tmp"' EXIT; curl --proto '=https' --tlsv1.2 -fL "https://github.com/yiiilin/termd/releases/latest/download/${asset}" -o "$tmp"; chmod 0755 "$tmp"; sudo "$tmp" install)
```

首次安装默认启用 Web 并监听 `127.0.0.1:8080`。安装完成后会打印 `SENSITIVE relay setup token`。先保存这个 token，再按 [TLS 与反向代理](docs/deployment.md#tls-与反向代理)把自己的 HTTPS 域名转发到该地址。不要把 token 放进 URL、聊天记录或普通日志。

### 2. 安装 termd

在保存 shell session 的主机，用普通登录用户的 shell 执行：

```bash
(set -eu; case "$(uname -m)" in x86_64|amd64) arch=amd64 ;; aarch64|arm64) arch=arm64 ;; *) echo "仅支持 Linux amd64/arm64" >&2; exit 1 ;; esac; asset="termd-linux-${arch}"; tmp="$(mktemp)"; trap 'rm -f -- "$tmp"' EXIT; curl --proto '=https' --tlsv1.2 -fL "https://github.com/yiiilin/termd/releases/latest/download/${asset}" -o "$tmp"; chmod 0755 "$tmp"; sudo "$tmp" install)
```

首次安装默认启用 Web 和 `127.0.0.1:8765`，并询问是否使用当前 `SUDO_USER` 运行 shell session。随后选择本机直连或输入 relay 的 `wss://` 地址；选择 relay 时，setup token 采用隐藏输入。安装器验证服务后打印配对二维码和 `termd-pair:v2:...` 邀请码。本机模式打开 <http://127.0.0.1:8765>；relay 模式打开自己的 HTTPS relay 地址。

### 3. 安装 termctl

`termctl` 是可选的命令行客户端。在需要使用它的 Linux 主机执行：

```bash
(set -eu; case "$(uname -m)" in x86_64|amd64) arch=amd64 ;; aarch64|arm64) arch=arm64 ;; *) echo "仅支持 Linux amd64/arm64" >&2; exit 1 ;; esac; asset="termctl-linux-${arch}"; tmp="$(mktemp)"; trap 'rm -f -- "$tmp"' EXIT; curl --proto '=https' --tlsv1.2 -fL "https://github.com/yiiilin/termd/releases/latest/download/${asset}" -o "$tmp"; chmod 0755 "$tmp"; sudo "$tmp" install)
```

邀请码过期后，在 termd 主机重新生成：

```bash
termd pair --qr
```

### 使用代理下载

下载前设置标准代理变量即可：

```bash
export http_proxy=http://127.0.0.1:7890 https_proxy=http://127.0.0.1:7890 no_proxy=127.0.0.1,localhost
```

`curl` 会直接读取这些变量。通过 `sudo` 执行 `upgrade` 时，需要按本机 sudo policy 保留这些变量，例如 `sudo --preserve-env=http_proxy,https_proxy,no_proxy termd upgrade`。如果 termd 运行后也必须通过代理连接 relay，把第 2 步单行命令末尾的 `install` 替换成 `install --proxy http://127.0.0.1:7890`（保留最后的 `)`）。

## 升级

按安装顺序升级即可。程序会探测最新版本、校验 GitHub 提供的 SHA-256 digest、替换二进制并检查服务：

```bash
sudo termrelay upgrade
sudo termd upgrade
sudo termctl upgrade
```

普通升级会保留配置、daemon identity、已配对设备和 session。只有 supervisor compatibility 确实变化时，`termd` 才会单独警告 session 将丢失并要求第二次确认；`--yes` 不会代替这次确认。

## 卸载

默认卸载保留本地状态：

```bash
sudo termctl uninstall
sudo termd uninstall
sudo termrelay uninstall
```

`termd uninstall --purge` 和 `termrelay uninstall --purge` 会不可恢复地删除对应状态，执行前必须自行备份。公网端口、Nginx/OpenResty 和 Docker Compose 配置见[公网部署方案](docs/deployment.md)。

## License

MIT. See [LICENSE](LICENSE) and [Third-Party Notices](THIRD_PARTY_NOTICES.md).
