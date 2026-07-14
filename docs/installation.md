# 安装、升级与卸载

本文是 termd 的完整安装入口。新用户先选择访问拓扑，再按对应步骤安装。公网反向代理的完整配置见[公网部署方案](deployment.md)。

## 支持范围

| 平台 | 安装方式 | 说明 |
| --- | --- | --- |
| Linux x86_64/amd64 + systemd | 预编译 release | 推荐；安装器校验 release checksum |
| Linux arm64/aarch64 + systemd | 源码编译 fallback | 需要 Rust 1.85+；Web 构建还需要 Node.js 22 + npm |
| 非 Linux 或无 systemd | 不支持一键安装 | 可用于开发，但本文的 service 命令不适用 |

安装器必须以 root 运行，并依赖 `install`、`tar`、`sha256sum`、`python3`、`systemctl`、`useradd` 以及 `curl` 或 `wget`。Ubuntu/Debian x86_64 可先安装基础依赖：

```bash
sudo apt-get update
sudo apt-get install -y \
  ca-certificates coreutils curl findutils gawk grep passwd python3 sed tar
```

x86_64 安装器在 release archive 或 checksum 无法下载、缺失或校验失败时也会回退到源码编译。要让该回退可用，还需预装 Git、Rust 1.85+ 和系统构建工具；使用 `--web` 时还需 Node.js 22 与 npm。否则安装器会停止并保留原有安装，不会跳过 checksum 继续使用下载文件。

## 通过代理安装

三个 release installer 都支持标准的 `http_proxy`、`https_proxy`、`all_proxy` 和 `no_proxy` 环境变量，也接受对应的大写形式。大小写同时存在时以小写值为准。代理会用于 release 查询、archive/checksum 下载，以及 arm64 或下载失败后的 Git、Cargo 和 npm 源码构建。

`sudo` 默认可能丢弃代理变量。先在普通用户 shell 导出变量，再只允许 sudo 保留这些变量：

```bash
export http_proxy=http://127.0.0.1:7890
export https_proxy="$http_proxy"
export no_proxy=127.0.0.1,localhost

curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo --preserve-env=http_proxy,https_proxy,all_proxy,no_proxy \
      bash -s -- --web --user "$(id -un)"
```

第一段 `curl` 使用当前用户导出的代理，sudo 后的 installer 继续使用同一组变量。`no_proxy` 应至少保留 `127.0.0.1,localhost`，否则安装后的本机 health、pairing 和 relay 注册检查可能被送进代理。安装 `termrelay` 或 `termctl` 时使用相同写法，只替换 installer URL 和参数。

这些变量负责安装过程的联网。若 `termd` 运行后连接 relay 也必须长期经过代理，安装时另加 `--proxy <URL>`，或在 `/etc/termd/termd.env` 中配置 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY` 后重启服务。包含账号密码的代理 URL 属于敏感信息，不要写入 issue、聊天或共享日志。

### Linux arm64

arm64 没有预编译 release archive。安装器会 clone 当前 release tag 并执行 `cargo build --release --locked`：

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential ca-certificates coreutils curl findutils gawk git grep passwd python3 sed tar
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
rustc --version
```

`rustc --version` 必须是 1.85 或更高。启用 `--web` 前还要按 Node.js 官方方式安装 Node.js 22，并确认：

```bash
node --version
npm --version
```

从普通用户 shell 运行 arm64 安装器时，把该用户的 Rust 路径明确传给 sudo：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo env \
      CARGO_HOME="$HOME/.cargo" \
      RUSTUP_HOME="$HOME/.rustup" \
      PATH="$HOME/.cargo/bin:$PATH" \
      bash -s -- --web --user "$(id -un)"
```

不需要 Web 时删除 `--web`，也不需要 Node.js/npm。源码编译需要的时间和磁盘明显多于 x86_64 预编译安装。

## 选择连接方式

| 使用场景 | 推荐方式 | 是否直接暴露 daemon |
| --- | --- | --- |
| daemon 主机本机使用 | loopback 直连 | 否 |
| 从自己的电脑临时访问服务器 | SSH tunnel | 否 |
| 可信 LAN 或 VPN 内访问 | LAN 监听 | 是，仅明文 HTTP |
| 从公网长期访问 | 两主机 trusted relay | daemon 否；relay 经 TLS 暴露 |

`127.0.0.1` 只能从 daemon 主机自身访问。`0.0.0.0` 表示监听所有网卡，不是浏览器访问地址。公网不要直接暴露 daemon 的 8765 端口或 `/local/pairing-token`。

## 本机直连

从要运行 session 的普通 Linux 用户 shell 执行：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --web --user "$(id -un)"
```

`--user "$(id -un)"` 让 Web 创建的 session 使用当前用户的 HOME 和 login shell。省略它会使用受限的 `termd` system user。

验证二进制、service 和 HTTP health：

```bash
termd --version
sudo systemctl is-enabled termd
sudo systemctl is-active termd
curl -fsS http://127.0.0.1:8765/healthz | python3 -m json.tool
```

预期 service 为 `enabled`、`active`，health JSON 的 `status` 为 `ok`。打开 <http://127.0.0.1:8765>，粘贴安装器打印的 `termd-pair:v2:...` 邀请码。

邀请码过期后重新签发：

```bash
termd pair --qr
```

## 通过 SSH tunnel 访问

daemon 保持默认的 `127.0.0.1:8765` 监听。在浏览器所在电脑建立隧道：

```bash
ssh -N -L 8765:127.0.0.1:8765 alice@terminal-host
```

上面的 `alice@terminal-host` 是占位 SSH 目标，**必须替换，不能原样执行**。隧道保持运行时，在本机浏览器打开 <http://127.0.0.1:8765>。邀请码仍在 daemon 主机用 `termd pair --qr` 签发。

如果本机 8765 已占用，可把左侧端口改成其他值，例如 `-L 18765:127.0.0.1:8765`，浏览器相应打开 `http://127.0.0.1:18765`。

## 可信 LAN 或 VPN

仅在确认网络可信且有防火墙限制时使用：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --web --user "$(id -un)" --listen 0.0.0.0:8765
```

从其他设备打开 `http://DAEMON_LAN_IP:8765`。`DAEMON_LAN_IP` 是占位符，**必须替换成 daemon 的真实 LAN/VPN 地址，不能原样使用**。该路径是明文 HTTP，可信 relay 也不是它的自动 TLS 层；不要通过路由器端口转发把 8765 暴露到互联网。

验证监听和本机 health：

```bash
sudo ss -ltnp | grep ':8765'
curl -fsS http://127.0.0.1:8765/healthz | python3 -m json.tool
```

用主机防火墙只允许实际 LAN/VPN 网段访问 TCP 8765。防火墙命令因发行版和网络而异，设置前先确认管理 SSH 不会被阻断。

## 公网 trusted relay：两主机流程

推荐使用两台主机：

- relay 主机：具有公网域名和有效 TLS 证书，只运行 `termrelay` 与反向代理。
- daemon 主机：运行 `termd` 和真实 shell/session，只需向 relay 发起出站连接。

下面的 `relay.example.com`、`alice@terminal-host` 和 `/secure/received/...` 都是占位值，**必须替换，不能原样执行**。

### 1. 安装 relay

在 relay 主机执行：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh \
  | sudo bash -s -- --web --listen 127.0.0.1:8080

termrelay --version
sudo systemctl is-active termrelay
curl -fsS http://127.0.0.1:8080/healthz | python3 -m json.tool
sudo ls -l /etc/termd/termrelay_setup_token
```

安装器创建 `/etc/termd/termrelay_setup_token` 和 `/var/lib/termrelay/daemon-registry.json`。setup token 只用于注册 daemon，不要放进 URL、argv、聊天或日志。

按[公网部署方案](deployment.md#tls-与反向代理)配置 `relay.example.com:443` 的 TLS 反向代理，再验证：

```bash
curl -fsS https://relay.example.com/healthz | python3 -m json.tool
```

`relay.example.com` 必须先替换成真实域名。

### 2. 安全传送 setup token

通过 SSH、密码管理器或其他受控通道，把 relay 主机的 setup token 文件内容传到 daemon 主机。daemon 主机上的目标必须是 root-only 临时文件。例如收到文件后：

```bash
sudo install -m 0600 /secure/received/termrelay_setup_token \
  /run/termd-relay-setup-token
```

`/secure/received/termrelay_setup_token` 是占位路径，必须替换。不要把 token 直接写在 shell 命令行中。

### 3. 安装并注册 daemon

在 daemon 主机的普通登录用户 shell 执行：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- \
      --web \
      --user "$(id -un)" \
      --relay wss://relay.example.com \
      --relay-setup-token-file /run/termd-relay-setup-token
sudo rm -f /run/termd-relay-setup-token
```

`wss://relay.example.com` 必须替换成真实 relay URL。安装器会生成 `/etc/termd/termd_daemon_token`，并把 daemon token hash 与 daemon public key 注册到 relay；它不会把 pair ticket、device certificate 或设备私钥存到 relay。

### 4. 端到端验证与配对

```bash
termd --version
sudo systemctl is-active termd
curl -fsS http://127.0.0.1:8765/healthz | python3 -m json.tool
curl -fsS https://relay.example.com/healthz | python3 -m json.tool
```

把域名替换后，在浏览器打开 `https://relay.example.com`。在 daemon 主机运行 `termd pair --qr`，将同一份邀请码粘贴到 relay Web。浏览器只持久保存自己的 device credential；后续短期 access token 会自动刷新。

## 可选 termctl

`termctl` 主要用于配对和诊断，不是 Web 的替代安装条件：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh \
  | sudo bash
termctl --version
```

## 升级

### 本机直连或 LAN

重复运行原安装命令。显式传入的 `--web`、`--listen`、`--user` 会更新对应设置；未传入的设置沿用 `/etc/termd/termd.env` 和现有 systemd user。

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --web --user "$(id -un)"
```

升级前后都执行：

```bash
termd --version
sudo systemctl is-active termd
curl -fsS http://127.0.0.1:8765/healthz | python3 -m json.tool
```

installer 默认保留 daemon identity、已配对设备、配置和 `/var/lib/termd`。supervisor compatibility 相同时，daemon 重启不会终止已有 supervisor；兼容性确实改变时，installer 会说明 session 丢失范围并要求交互确认。不要通过删除数据库、socket 或 supervisor 进程绕过确认。

### 公网 relay

顺序固定为：

1. 在 relay 主机重复运行 `install-termrelay.sh`。
2. 验证本机和公网 `/healthz`。
3. 安全复制 setup token 到 daemon 主机的 root-only 临时文件。
4. 在 daemon 主机重复运行 `install-termd.sh --relay ... --relay-setup-token-file ...`。
5. 删除临时 setup token，并验证 daemon health、relay health 和 Web attach。

这样 relay registry 会包含当前 daemon public key。不要只升级 Web/relay 而长期混跑不同 release 的 daemon 和 client。

## 卸载

默认卸载程序与 service，但保留状态，便于重装：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --uninstall
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh \
  | sudo bash -s -- --uninstall
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh \
  | sudo bash -s -- --uninstall
```

`--purge` 会不可恢复地删除 identity、已配对设备、registry 和 session 状态，并可能终止仍在使用的 session。只有确认备份和影响后才执行：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --uninstall --purge
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh \
  | sudo bash -s -- --uninstall --purge
```

## 故障排查

### service 或 health 失败

```bash
sudo systemctl status termd --no-pager -l
sudo journalctl -u termd -n 200 --no-pager
sudo systemctl status termrelay --no-pager -l
sudo journalctl -u termrelay -n 200 --no-pager
```

只检查实际安装的 service。另可确认配置和监听：

```bash
sudo sed -n '1,200p' /etc/termd/termd.env
sudo ss -ltnp | grep -E ':(8765|8080)'
```

不要把 token 文件内容或 pairing invite 粘贴到 issue、聊天或日志中。

### 旧安装器报 `BASH_SOURCE[0]: unbound variable`

这是旧 release installer 从 stdin/pipe 执行时的错误。使用包含该修复的新 release，再按本文的 `curl ... | sudo bash -s -- ...` 命令重试。不要为绕过它改成来源不明的脚本副本。

### 旧 0.8.1 fresh install 反复重启

如果 journal 包含 `created before schema version 3`，且这是从未成功启动、从未创建 identity/device/session 的 fresh install，旧 installer 可能只写入了 `supervisor_version` 而没有让 daemon 初始化 schema。新版 installer 会在重装时自动识别这个精确的空状态形态：只移除错误 baseline，让 daemon 初始化 schema，再写回当前 supervisor baseline。

直接重跑新版安装命令，然后验证 service 和 health。自愈不会删除数据库、identity、设备、session 或 socket；只要检测到额外 meta、任何业务数据或 supervisor socket，它就拒绝修改并回滚安装。此时查看 journal 并保留 `/var/lib/termd` 交给人工诊断，**不要直接 `--purge`**。

### arm64 回退源码后提示缺少命令

- `missing required command: cargo`：安装 Rust 1.85+，并按 arm64 命令把 `CARGO_HOME`、`RUSTUP_HOME` 和 `PATH` 传给 sudo。
- `missing required command: node` 或 `npm`：使用 `--web` 时安装 Node.js 22 与 npm；不需要 Web 时去掉 `--web`。
- clone/download 失败：确认 GitHub 访问、系统时间、CA 证书和代理设置。

### relay 已启动但 daemon 不在线

确认 daemon 配置只包含一个 relay URL，并检查两端日志：

```bash
sudo grep -E '^(TERMD_RELAY_URLS|TERMD_RELAY_DAEMON_TOKEN_FILE)=' /etc/termd/termd.env
sudo journalctl -u termd -n 200 --no-pager
sudo journalctl -u termrelay -n 200 --no-pager
```

setup token 仅用于注册；daemon 长期连接使用 `/etc/termd/termd_daemon_token`。反向代理必须转发 `/ws`、`/ws/metadata`、`/ws/terminal`、`/api/auth/*`、`/api/control/*` 和 `/api/files/*`。

## 重要路径

| 路径 | 内容 |
| --- | --- |
| `/etc/termd/termd.env` | daemon 非敏感运行配置和 secret 文件路径 |
| `/etc/termd/termd_daemon_token` | daemon 到 trusted relay 的 admission token |
| `/var/lib/termd` | identity、设备、SQLite 状态和 supervisor runtime |
| `/etc/termd/termrelay.env` | relay 配置 |
| `/etc/termd/termrelay_setup_token` | daemon 注册 setup token |
| `/var/lib/termrelay/daemon-registry.json` | trusted daemon registry |

这些路径默认由 installer 管理。修改 env 后使用 `sudo systemctl restart termd` 或 `sudo systemctl restart termrelay`，再重新执行 health 和 journal 检查。
