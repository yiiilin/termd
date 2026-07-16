# 安装、升级与卸载

本文是 termd 的完整安装入口。新用户先选择访问拓扑，再按对应步骤安装。公网反向代理的完整配置见[公网部署方案](deployment.md)。

## 支持范围

| 平台 | 安装方式 | 说明 |
| --- | --- | --- |
| Linux x86_64/amd64 + systemd | 预编译 release 裸二进制 | `*-linux-amd64` |
| Linux arm64/aarch64 + systemd | 预编译 release 裸二进制 | `*-linux-arm64` |
| 非 Linux 或无 systemd | 不支持一键安装 | 可用于开发，但本文的 service 命令不适用 |

实际安装必须以 root 运行，并依赖 `install`、`python3`、`systemctl` 和 `useradd`；首次下载还需要 `curl`。Ubuntu/Debian 可先安装基础依赖：

```bash
sudo apt-get update
sudo apt-get install -y \
  ca-certificates coreutils curl findutils gawk grep passwd python3 sed
```

项目上传的 GitHub Release assets 恰好是三个组件在 amd64、arm64 上的六个裸二进制；项目不上传自有的 `tar.gz`、checksum 文件或安装脚本。GitHub 自动生成的 Source code（zip/tar.gz）归档不属于项目上传的 assets，也无法由 release workflow 禁用。每个二进制都自带 `install`、`uninstall` 和 `upgrade`。首次 bootstrap 依赖 GitHub HTTPS；安装后的 `upgrade` 会从 GitHub API 读取 asset digest 并强制执行 SHA-256 校验。

## 下载 Linux release

先选择当前架构，再把 `component` 设为 `termd`、`termrelay` 或 `termctl`：

```bash
case "$(uname -m)" in
  x86_64|amd64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
component=termd
asset="${component}-linux-${arch}"
curl --proto '=https' --tlsv1.2 -fL \
  "https://github.com/yiiilin/termd/releases/latest/download/${asset}" -o "$asset"
chmod 0755 "$asset"
"./$asset" --version
"./$asset" install --help
```

确认组件名和版本正确后再运行安装。后续示例假设本 shell 中的 `arch` 仍为上面得到的 `amd64` 或 `arm64`。安装完成后可以删除下载文件。首次下载的信任根是系统 CA 与 GitHub HTTPS；以后优先使用内置 `upgrade`，它还会验证 release API 中 `sha256:<64hex>` 格式的 asset digest。

## 通过代理安装

`curl` 和内置 `upgrade` 都支持标准的 `http_proxy`、`https_proxy`、`all_proxy` 和 `no_proxy` 环境变量，也接受对应的大写形式。内嵌 installer 直接复制当前二进制，不再联网，因此首次安装不需要把下载代理传给 sudo。

先在普通用户 shell 导出代理，再执行上一节的下载和校验命令：

```bash
export http_proxy=http://127.0.0.1:7890
export https_proxy="$http_proxy"
export no_proxy=127.0.0.1,localhost

component=termd
asset="${component}-linux-${arch}"
curl --proto '=https' --tlsv1.2 -fL \
  "https://github.com/yiiilin/termd/releases/latest/download/${asset}" -o "$asset"
chmod 0755 "$asset"
sudo "./$asset" install --web --user "$(id -un)"
```

`curl` 使用当前用户导出的代理；内嵌 installer 不使用该下载代理。`no_proxy` 应至少保留 `127.0.0.1,localhost`，避免后续本机 health、pairing 和 relay 注册检查被送进代理。安装 `termrelay` 或 `termctl` 时只替换 `component` 和安装参数。

这些变量负责安装过程的联网。若 `termd` 运行后连接 relay 也必须长期经过代理，安装时另加 `--proxy <URL>`，或在 `/etc/termd/termd.env` 中配置 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY` 后重启服务。包含账号密码的代理 URL 属于敏感信息，不要写入 issue、聊天或共享日志。

## 源码构建与兼容脚本

源码构建出的三个二进制同样包含 installer。完成 Web 构建和 Rust release build 后，可先用无副作用计划检查参数，再执行安装：

```bash
(cd termui/frontend && npm ci && npm run build)
cargo build --release --locked -p termd -p termrelay -p termctl
target/release/termd install --dry-run --web --user "$(id -un)"
sudo target/release/termd install --web --user "$(id -un)"
```

仓库中的 `scripts/install-*.sh` 只作为源码开发和旧自动化兼容工具保留，不是 Release 资产，也不是 amd64 或 arm64 的官方安装入口。非交互自编译安装可在审阅 `--dry-run` 后传 `--yes`；supervisor compatibility 确实变化且允许丢失 session 时，仍必须单独显式传 `--allow-session-loss`。

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
./termd-linux-${arch} install --dry-run --web --user "$(id -un)"
sudo ./termd-linux-${arch} install --web --user "$(id -un)"
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
./termd-linux-${arch} install --dry-run --web --user "$(id -un)" --listen 0.0.0.0:8765
sudo ./termd-linux-${arch} install --web --user "$(id -un)" --listen 0.0.0.0:8765
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
relay 与 daemon 是两台不同主机时，应在各自主机先执行[下载 Linux release](#下载-linux-release)里的架构检测和下载步骤：relay 主机选择 `component=termrelay`，daemon 主机选择 `component=termd`；shell 变量和下载文件不会跨主机共享。

### 1. 安装 relay

在 relay 主机执行：

```bash
./termrelay-linux-${arch} install --dry-run --web --listen 127.0.0.1:8080
sudo ./termrelay-linux-${arch} install --web --listen 127.0.0.1:8080

termrelay --version
sudo systemctl is-active termrelay
curl -fsS http://127.0.0.1:8080/healthz | python3 -m json.tool
```

安装器创建或保留 `/etc/termd/termrelay_setup_token` 和 `/var/lib/termrelay/daemon-registry.json`，并在成功结尾直接打印当前 setup token（新装和升级都一样）。输出会明确标记为敏感值；只通过 SSH 终端、密码管理器等受控通道传给 daemon 主机，不要放进 URL、聊天或普通日志。旧版无鉴权配置在升级时会自动迁移到这一 trusted 配置，不会继续保留无鉴权入口。

按[公网部署方案](deployment.md#tls-与反向代理)配置 `relay.example.com:443` 的 TLS 反向代理，再验证：

```bash
curl -fsS https://relay.example.com/healthz | python3 -m json.tool
```

`relay.example.com` 必须先替换成真实域名。

### 2. 安装 daemon，并隐藏输入 setup token

在 daemon 主机的普通登录用户 shell 执行：

```bash
./termd-linux-${arch} install --dry-run \
  --web \
  --user "$(id -un)" \
  --relay wss://relay.example.com
sudo ./termd-linux-${arch} install \
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

token 文件方式示例：

```bash
sudo install -m 0600 /secure/received/termrelay_setup_token /run/termd-relay-setup-token
sudo ./termd-linux-${arch} install \
  --yes \
  --web \
  --user "$(id -un)" \
  --relay wss://relay.example.com \
  --relay-setup-token-file /run/termd-relay-setup-token
sudo rm -f /run/termd-relay-setup-token
```

`/secure/received/termrelay_setup_token` 是占位路径，必须替换。直接 token 与 token 文件不能同时提供。未显式传 `--relay` 的普通升级不会要求重新输入 setup token。

### 4. 端到端验证与配对

注册完成后，安装器读取本机 `/healthz` 的 `server_id`，再携带 setup token 向 relay 查询这个 id 的 control 连接；不会用其他 daemon 在线或全局连接数冒充。成功时打印 `SUCCESS: daemon ... is connected to relay ...`；超时则打印 `FAILED`。随后安装器实际执行 `termd pair --qr --url <LOCAL_URL>`，直接输出二维码和 `termd-pair:v2:...` 邀请码。

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
./termctl-linux-${arch} install --dry-run
sudo ./termctl-linux-${arch} install
termctl --version
```

## 升级

### 本机直连或 LAN

已安装的三个程序都能探测 GitHub latest release。升级器严格比较 semver；有新版本时显示 current、latest 和目标 asset，确认后下载当前架构的裸二进制、校验 release API 提供的 SHA-256 digest，并运行候选程序的 `--version`。全部通过后，候选程序调用自身的 managed installer 原子替换现有文件；`termd`、`termrelay` 会重启并检查 systemd service，`termctl` 只替换程序。

```bash
sudo termd upgrade
sudo termctl upgrade
```

没有新版本时命令清楚报告当前已是最新版并成功退出。拒绝确认时不下载、不替换也不重启。非交互自动化使用 `--yes`。升级保留安装 prefix、daemon identity、已配对设备、配置和 `/var/lib/termd`；显式 `TERMD_INSTALL_PREFIX` 优先，否则从 `/prefix/bin/<component>` 推导，无法推导时使用 `/usr/local`。

升级前后都执行：

```bash
termd --version
sudo systemctl is-active termd
curl -fsS http://127.0.0.1:8765/healthz | python3 -m json.tool
```

supervisor compatibility 相同时，daemon 重启不会终止已有 supervisor。兼容性改变时，`termd` 的新 installer 会额外明确警告“现有 session 会丢失”并要求第二次确认。普通 `--yes` 只确认升级本身，不能授权丢失 session；经过外部备份和影响确认的非交互升级必须明确运行 `sudo termd upgrade --yes --allow-session-loss`。不要通过删除数据库、socket 或 supervisor 进程绕过确认。

升级 HTTP 请求沿用标准 proxy 环境。`sudo` 默认可能丢弃这些变量，需要代理时按本机 sudo policy 显式保留，例如：

```bash
sudo --preserve-env=http_proxy,https_proxy,all_proxy,no_proxy termd upgrade
```

### 公网 relay

从仍支持 open relay 的旧版本升级属于 breaking 迁移：`--allow-open-relay` 已删除，当前
binary 和 installer 都会把它作为 unknown argument 拒绝。只有 managed installer 会自动
移除旧 `TERMRELAY_ALLOW_OPEN_RELAY` 环境项，并创建或保留 setup token file 与 daemon
registry。手工维护 binary、systemd unit 或 container 的部署，必须在启动新版本前配置
可读且非空的 setup token file 和 registry path；不能继续沿用无鉴权启动参数。

managed installer 完成迁移后会打印 setup token。只在从旧 open relay 配置进行该 breaking 迁移时，既有 daemon 才需要使用该 token 重新执行 `sudo termd install --relay wss://relay.example.com`，完成注册并看到 `SUCCESS` 后再继续。

顺序固定为：

1. 在 relay 主机运行 `sudo termrelay upgrade`。
2. 验证本机和公网 `/healthz`；仅 breaking admission 迁移需要安全复制新打印的 setup token。
3. 在 daemon 主机运行 `sudo termd upgrade`。
4. 验证 daemon health、relay health 和 Web attach；需要时再运行 `sudo termctl upgrade`。

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

这是旧 release installer 从 stdin/pipe 执行时的错误。amd64 和 arm64 都应改用本文对应架构的裸二进制及其内嵌 installer，不要从 Release 下载旧脚本，也不要为绕过错误改用来源不明的脚本副本。

### 旧 0.8.1 fresh install 反复重启

如果 journal 包含 `created before schema version 3`，且这是从未成功启动、从未创建 identity/device/session 的 fresh install，旧 installer 可能只写入了 `supervisor_version` 而没有让 daemon 初始化 schema。新版 installer 会在重装时自动识别这个精确的空状态形态：只移除错误 baseline，让 daemon 初始化 schema，再写回当前 supervisor baseline。

直接重跑新版安装命令，然后验证 service 和 health。自愈不会删除数据库、identity、设备、session 或 socket；只要检测到额外 meta、任何业务数据或 supervisor socket，它就拒绝修改并回滚安装。此时查看 journal 并保留 `/var/lib/termd` 交给人工诊断，**不要直接 `--purge`**。

### upgrade 无法查询或校验 release

- 查询失败：确认 GitHub API 可达、系统时间和系统 CA 正常，并检查标准 proxy 环境是否被 sudo 保留。
- `missing required asset`：确认 release 同时发布了对应组件的 `linux-amd64` 或 `linux-arm64` 裸二进制。
- `must provide digest sha256` 或 `SHA-256 verification failed`：停止升级；不要跳过完整性校验或手工替换程序。
- `candidate identity mismatch`：asset 的组件名或版本与 release tag 不一致，停止并检查 release。

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
