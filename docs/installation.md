# 安装、升级与卸载

本文是 termd 的完整安装入口。新用户先选择访问拓扑，再按对应步骤安装。公网反向代理的完整配置见[公网部署方案](deployment.md)。

## 支持范围

| 平台 | 安装方式 | 说明 |
| --- | --- | --- |
| Linux x86_64/amd64 + systemd | 预编译 release | 推荐；下载后先校验 release checksum，再运行内嵌 installer |
| Linux arm64/aarch64 + systemd | 源码编译 fallback | 需要 Rust 1.85+；Web 构建还需要 Node.js 22 + npm |
| 非 Linux 或无 systemd | 不支持一键安装 | 可用于开发，但本文的 service 命令不适用 |

实际安装必须以 root 运行，并依赖 `install`、`python3`、`systemctl` 和 `useradd`；下载和校验还需要 `curl` 与 `sha256sum`。Ubuntu/Debian x86_64 可先安装基础依赖：

```bash
sudo apt-get update
sudo apt-get install -y \
  ca-certificates coreutils curl findutils gawk grep passwd python3 sed tar
```

x86_64 的主流程直接下载自带 installer 的 release binary；checksum 缺失或校验失败时必须停止，不能继续执行。兼容 `install-*.sh` 仍会在预编译 archive 不可用时回退到源码编译；该路径还需 Git、Rust 1.85+、系统构建工具，以及 Web 构建所需的 Node.js 22 与 npm。

## 下载并校验 Linux amd64 release

先把 `component` 设为当前要安装的 `termd`、`termrelay` 或 `termctl`，再逐步下载、校验并检查帮助：

```bash
component=termd
curl -fL "https://github.com/yiiilin/termd/releases/latest/download/${component}-linux-amd64" \
  -o "${component}-linux-amd64"
curl -fL https://github.com/yiiilin/termd/releases/latest/download/checksums.txt \
  -o checksums.txt
sha256sum --ignore-missing --check checksums.txt
chmod 0755 "${component}-linux-amd64"
"./${component}-linux-amd64" --version
"./${component}-linux-amd64" install --help
```

checksum 输出必须包含对应文件的 `OK`；否则删除下载文件并停止。后续各节使用同一目录里的已校验二进制。安装完成后可以删除下载文件和 `checksums.txt`。

## 通过代理安装

`curl` 支持标准的 `http_proxy`、`https_proxy`、`all_proxy` 和 `no_proxy` 环境变量，也接受对应的大写形式。内嵌 installer 直接复制当前已校验二进制，不再联网，因此不需要把下载代理传给 sudo。兼容 `install-*.sh` 也支持这些变量，并会把它们用于 release 下载及 arm64 的 Git、Cargo 和 npm 源码构建。

先在普通用户 shell 导出代理，再执行上一节的下载和校验命令：

```bash
export http_proxy=http://127.0.0.1:7890
export https_proxy="$http_proxy"
export no_proxy=127.0.0.1,localhost

component=termd
curl -fL "https://github.com/yiiilin/termd/releases/latest/download/${component}-linux-amd64" \
  -o "${component}-linux-amd64"
curl -fL https://github.com/yiiilin/termd/releases/latest/download/checksums.txt \
  -o checksums.txt
sha256sum --ignore-missing --check checksums.txt
chmod 0755 "${component}-linux-amd64"
sudo ./termd-linux-amd64 install --web --user "$(id -un)"
```

两个 `curl` 使用当前用户导出的代理；内嵌 installer 不使用该下载代理。`no_proxy` 应至少保留 `127.0.0.1,localhost`，避免后续本机 health、pairing 和 relay 注册检查被送进代理。安装 `termrelay` 或 `termctl` 时只替换 `component` 和安装参数。

这些变量负责安装过程的联网。若 `termd` 运行后连接 relay 也必须长期经过代理，安装时另加 `--proxy <URL>`，或在 `/etc/termd/termd.env` 中配置 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY` 后重启服务。包含账号密码的代理 URL 属于敏感信息，不要写入 issue、聊天或共享日志。

## 源码构建与兼容脚本

源码构建出的三个二进制同样包含 installer。完成 Web 构建和 Rust release build 后，可先用无副作用计划检查参数，再执行安装：

```bash
(cd termui/frontend && npm ci && npm run build)
cargo build --release --locked -p termd -p termrelay -p termctl
target/release/termd install --dry-run --web --user "$(id -un)"
sudo target/release/termd install --web --user "$(id -un)"
```

`install-*.sh` 继续作为 arm64 源码 fallback 和旧自动化的兼容入口。非交互环境可在审阅 `--dry-run` 后传 `--yes`；它只确认普通安装计划。supervisor compatibility 确实变化且允许丢失 session 时，仍必须单独显式传 `--allow-session-loss`。

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
./termd-linux-amd64 install --dry-run --web --user "$(id -un)"
sudo ./termd-linux-amd64 install --web --user "$(id -un)"
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
./termd-linux-amd64 install --dry-run --web --user "$(id -un)" --listen 0.0.0.0:8765
sudo ./termd-linux-amd64 install --web --user "$(id -un)" --listen 0.0.0.0:8765
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
./termrelay-linux-amd64 install --dry-run --web --listen 127.0.0.1:8080
sudo ./termrelay-linux-amd64 install --web --listen 127.0.0.1:8080

termrelay --version
sudo systemctl is-active termrelay
curl -fsS http://127.0.0.1:8080/healthz | python3 -m json.tool
```

安装器创建 `/etc/termd/termrelay_setup_token` 和 `/var/lib/termrelay/daemon-registry.json`，并在成功结尾直接打印当前 setup token（新装和升级都一样）。输出会明确标记为敏感值；只通过 SSH 终端、密码管理器等受控通道传给 daemon 主机，不要放进 URL、聊天或普通日志。使用 `--allow-open-relay` 时没有 setup token，安装器会明确说明不适用。

按[公网部署方案](deployment.md#tls-与反向代理)配置 `relay.example.com:443` 的 TLS 反向代理，再验证：

```bash
curl -fsS https://relay.example.com/healthz | python3 -m json.tool
```

`relay.example.com` 必须先替换成真实域名。

### 2. 安装 daemon，并隐藏输入 setup token

在 daemon 主机的普通登录用户 shell 执行：

```bash
./termd-linux-amd64 install --dry-run \
  --web \
  --user "$(id -un)" \
  --relay wss://relay.example.com
sudo ./termd-linux-amd64 install \
  --web \
  --user "$(id -un)" \
  --relay wss://relay.example.com
```

dry-run 不读取 token，只会说明正式安装将询问。正式安装确认计划后显示 `Relay setup token (input hidden):`；粘贴 relay 安装器打印的 token 并回车，输入不会回显。`wss://relay.example.com` 必须替换成真实 relay URL。

安装器会生成 `/etc/termd/termd_daemon_token`，并把 daemon token hash 与 daemon public key 注册到 relay；setup token 只用于这次 identity 注册。relay 不保存 pair ticket、device certificate 或设备私钥。

### 3. 非交互安装

非交互执行必须显式选择一种 token 来源：

- `--relay-token <TOKEN>`：直接参数，适合受控自动化，但会短暂出现在调用进程 argv，且可能进入 shell history。
- `--relay-setup-token-file <PATH>`：推荐的自动化方式；文件应归 root 所有且权限为 `0600`。
- `--allow-open-relay`：仅当 relay 也明确使用 open 模式时与 `--relay` 同时传入；该模式不提示 setup token，也不能与两种 token 参数并用。

token 文件方式示例：

```bash
sudo install -m 0600 /secure/received/termrelay_setup_token /run/termd-relay-setup-token
sudo ./termd-linux-amd64 install \
  --yes \
  --web \
  --user "$(id -un)" \
  --relay wss://relay.example.com \
  --relay-setup-token-file /run/termd-relay-setup-token
sudo rm -f /run/termd-relay-setup-token
```

`/secure/received/termrelay_setup_token` 是占位路径，必须替换。直接 token 与 token 文件不能同时提供。未显式传 `--relay` 的普通升级不会要求重新输入 setup token。

### 4. 端到端验证与配对

注册完成后，安装器读取本机 `/healthz` 的 `server_id`，再向 relay 查询这个 id 的 control 连接；不会用其他 daemon 在线或全局连接数冒充。trusted relay 的查询仍需 setup token；显式 open relay 查询不带 token。成功时打印 `SUCCESS: daemon ... is connected to relay ...`；超时则打印 `FAILED`。随后安装器实际执行 `termd pair --qr --url <LOCAL_URL>`，直接输出二维码和 `termd-pair:v2:...` 邀请码。

本机 health、relay control 或 pairing 任一失败时，安装命令以非零状态结束，并打印 `local service installed but post-install verification/pairing failed`。这表示二进制和 systemd service 已落盘，不会伪装成回滚成功；按紧随其后的 `retry with:`/`then run:` 命令修复并重试。

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
./termctl-linux-amd64 install --dry-run
sudo ./termctl-linux-amd64 install
termctl --version
```

## 升级

### 本机直连或 LAN

重复运行原安装命令。显式传入的 `--web`、`--listen`、`--user` 会更新对应设置；未传入的设置沿用 `/etc/termd/termd.env` 和现有 systemd user。

```bash
./termd-linux-amd64 install --dry-run --web --user "$(id -un)"
sudo ./termd-linux-amd64 install --web --user "$(id -un)"
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

1. 在 relay 主机重新下载并校验 `termrelay-linux-amd64`，再运行其 `install` 子命令。
2. 验证本机和公网 `/healthz`。
3. 安全复制 setup token 到 daemon 主机的 root-only 临时文件。
4. 在 daemon 主机重新下载并校验 `termd-linux-amd64`，再按原参数运行其 `install` 子命令。
5. 删除临时 setup token，并验证 daemon health、relay health 和 Web attach。

这样 relay registry 会包含当前 daemon public key。不要只升级 Web/relay 而长期混跑不同 release 的 daemon 和 client。

## 卸载

默认卸载程序与 service，但保留状态，便于重装：

```bash
sudo termd uninstall
sudo termrelay uninstall
sudo termctl uninstall
```

`--purge` 会不可恢复地删除 identity、已配对设备、registry 和 session 状态，并可能终止仍在使用的 session。只有确认备份和影响后才执行：

```bash
sudo termd uninstall --purge
sudo termrelay uninstall --purge
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

这是旧 release installer 从 stdin/pipe 执行时的错误。amd64 请改用本文的已校验 raw binary 和内嵌 installer；arm64 请重新下载当前 `install-termd.sh`。不要为绕过它改用来源不明的脚本副本。

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
