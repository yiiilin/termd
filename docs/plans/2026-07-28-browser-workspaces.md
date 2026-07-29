# Browser Workspace 与按架构运行时

## 目标

在已配对的 termd Web 客户端中输入一个 `http` 或 `https` URL，创建独立的 Browser
Session，并在单独窗口中通过 noVNC 操作运行在 daemon 主机上的 Chromium。Browser Session
由 termd 管理，不属于任何 Terminal Session；client 或 relay 断开不会终止它。

首期支持 glibc `2.31+` 的 Linux `amd64` 与 `arm64`（例如 Ubuntu 20.04+、Debian 11+）。
Chromium 是唯一要求用户预先安装的外部程序；Xtigervnc、Openbox 及其非基础运行库由发布
CI 生成 runtime，termd 按架构下载，宿主只提供 glibc 等基础运行库。

## 明确不做

- 不把 Chromium、TigerVNC 或 Openbox 链接进 termd 主二进制。
- 不在 relay 保存 Browser Session、Chromium profile 或 RFB 状态。
- 不自行实现 VNC 编码、浏览器渲染或窗口管理器。
- 不支持目录式浏览、任意命令或非 HTTP(S) URL。
- 首期不承诺 Alpine/musl 宿主、32 位架构或超过四个并行 Browser Session。

## Domain Language

**Browser Session**：一组持久运行的 Xtigervnc、Openbox、Chromium 进程及独立 Chromium
profile。状态为 `CREATED -> RUNNING -> CLOSED`。

**Browser Runtime**：按 OS/架构发布的自包含运行包，包含 Xtigervnc、Openbox、必要动态库、
X11 数据、许可证和 manifest。它不是 Chromium，也不是 termd 主程序的一部分。

**RFB Attachment**：已认证 client 到某个运行中 Browser Session 的临时 RFB 字节流连接。
断开只 detach 当前连接。

## 模块与 Interface

`termd::browser::BrowserWorkspace` 是 daemon auth 与操作系统 GUI runtime 之间的 seam：

```rust
pub struct BrowserCreateRequest {
    pub url: String,
    pub width: u16,
    pub height: u16,
}

impl BrowserWorkspace {
    pub async fn list(&self) -> Result<Vec<BrowserSession>, BrowserError>;
    pub async fn create(&self, request: BrowserCreateRequest)
        -> Result<BrowserSession, BrowserError>;
    pub async fn close(&self, id: BrowserSessionId) -> Result<(), BrowserError>;
    pub async fn connect_rfb(&self, id: BrowserSessionId)
        -> Result<tokio::net::UnixStream, BrowserError>;
}
```

Interface 不暴露 display number、PID、profile 路径、runtime 路径或 Chromium flags。
Implementation 隐藏 runtime 安装、进程启动、持久记录、恢复、Unix socket 和清理。

依赖分类：

- in-process：URL/viewport/manifest 校验和状态机。
- local-substitutable：记录目录、runtime 目录、Unix socket。
- remote but owned：termrelay 的 opaque WebSocket pipe，测试使用内存/本地 relay adapter。
- true external：Xtigervnc、Openbox、Chromium；模块测试使用 fake runtime adapter。

## 进程与数据流

```text
Browser window / noVNC
  |  authenticated WebSocket, termd.rfb.v1
  v
termd direct endpoint or trusted relay opaque route
  |
  |  raw RFB bytes
  v
0600 Unix socket -> Xtigervnc -> X11 -> Openbox + Chromium
```

termd 通过内部 `__browser-supervisor` 子命令启动每个 Browser Session。supervisor 使用当前
termd 二进制，负责三个子进程和信号清理；systemd 的 `KillMode=process` 允许 supervisor 在
daemon 更新重启时继续存活。daemon 启动时根据私有记录、`/proc` 进程身份和 RFB socket
重新接回存活 session。每个 supervisor 是独立进程组 leader，所有 GUI 子进程继承
`TERMD_BROWSER_SESSION_ID`；关闭、启动回滚和异常恢复均对经 `/proc` 验证归属的整个进程组
执行 `TERM -> 限时等待 -> KILL`，确认组内成员全部退出后才清理记录。

创建采用 durable intent：daemon 先持久化 `CREATED + supervisor_pid=0`，启动 supervisor 后
写回 PID 并再次持久化，最后才写入 mode `0600` 的一次性 `start` marker。supervisor 未读到
与 Browser Session ID 匹配的 marker 前不启动任何 GUI 子进程，并在 25 秒后自行退出。这样
daemon 在 spawn 前后任一窗口崩溃时，要么能恢复已记录的 supervisor，要么只需清理无进程
intent，不会留下不计入容量且无法关闭的 Xtigervnc/Chromium。关闭和恢复清理均先停止进程、
可靠删除 run/profile/config，再删除持久记录；清理失败时保留记录供下次重试。
Xtigervnc 被强杀时可能留下 `/tmp/.X11-unix/X<display>` 与 `/tmp/.X<display>-lock`；termd
只处理持久记录中的 display，并在 socket/lock 类型、owner、inode、lock PID 已死亡以及 socket
无 listener 全部验证后删除。证据不完整或路径发生替换时不删除全局文件，并保留持久记录重试。

## HTTP 与 WebSocket Interface

所有 HTTP 操作要求现有 Bearer access token，并由 daemon 最终验证：

- `GET /api/browser/sessions`：列出未关闭 Browser Session。
- `POST /api/browser/sessions`：创建 session；body 为 `url/width/height`。
- `DELETE /api/browser/sessions/:browser_id`：显式关闭。
- `GET /ws/browser/:browser_id`：`termd.rfb.v1` + access token subprotocol；升级后只允许
  binary RFB frame。

relay 新增 `RelayRouteKind::Browser`，并在连接级路由元数据中携带 `browser_id`。relay 只
校验 transport access token、选择 daemon data pipe 和转发 frame，不查询 Browser Session。

## Runtime 包

Release 资产：

```text
termd-browser-runtime-linux-amd64.tar.gz
termd-browser-runtime-linux-amd64.json
termd-browser-runtime-linux-arm64.tar.gz
termd-browser-runtime-linux-arm64.json
```

归档布局：

```text
bin/Xtigervnc
bin/openbox
lib/
share/X11/
share/licenses/
```

manifest 固定 schema、termd 兼容版本、runtime 版本、OS、架构、最低 glibc、归档大小和
SHA-256。
下载使用同版本 GitHub Release，先写入私有临时目录，校验后进行拒绝路径穿越的结构化解包，
最后原子 rename。失败保留上一份完整 runtime，不覆盖 `current`。

发布 CI 在原生 `amd64`/`arm64` runner 的 Ubuntu 20.04 容器中，从固定 source archive 编译
TigerVNC 1.16.2 和 Openbox 3.6.1，并拒绝任何要求高于 `GLIBC_2.31` 的产物。job 只在 runtime
source/脚本变化、手工触发或 tag release 时执行。发布包保留 GPL/MIT/MPL 等第三方许可证、
source URL、checksum、glibc 基线和构建配方。

## 安全与资源约束

- URL 只接受绝对 `http`/`https`，最大 2048 bytes；允许 daemon 可达的内网地址。
- Xtigervnc 以 `-nolisten tcp` 禁止 X11 TCP，并禁用 TCP RFB 端口，只创建 mode `0600`
  Unix socket。
- RFB socket 遵守 Linux 107-byte `sun_path` 上限；state 路径过长时回退到同一 state parent
  下 mode `0700` 的短目录，仍超限则在写入 durable intent 前失败。
- 私有 UDS 后端使用 `SecurityTypes=None`；公网准入由 termd access token/WSS 完成。
- Chromium 不允许 `--no-sandbox`，每个 session 使用 mode `0700` 独立 profile。
- daemon 以 root 运行时，仅 Chromium 子进程降权为 uid/gid 均非 0 的无特权 `nobody` 账号并清空附加组；
  profile 位于 root 持有、不可列目录的 `/var/tmp/termd-browser-profiles` 下。非 root daemon
  保持当前 uid/gid。缺少安全降权条件时创建失败，不回退到 `--no-sandbox`。
- Browser Runtime 的 `LD_LIBRARY_PATH` 只应用于 Xtigervnc/Openbox，不传给宿主 Chromium，
  避免发行版运行库覆盖 Chromium 自身依赖。
- viewport 限制为 `640..3840 x 480..2160`，并行 Browser Session 最多四个。
- runtime manifest、归档路径、文件类型、大小和 SHA-256 全部校验；拒绝 symlink、hardlink、
  absolute path 与 `..`。
- runtime 只使用 termd 已安装并校验的版本，或管理员通过 `TERMD_BROWSER_RUNTIME_DIR`、成对
  的 `TERMD_BROWSER_XVNC`/`TERMD_BROWSER_OPENBOX` 显式指定的路径；不自动接受 `PATH` 中
  来源和参数兼容性未知的 `Xvnc`。
- RFB WebSocket 只接受 binary frame，并受现有 WebSocket frame/message 上限和背压约束。
- access token、daemon key、relay token、Chromium profile 内容不进入日志或错误响应。

## Web UI

主 workspace 工具栏和移动菜单增加 Browser 入口。管理对话框包含 URL 输入、创建按钮和当前
Browser Session 列表；每条 session 可在独立窗口重新打开或显式停止。独立页面只包含紧凑
工具栏和全尺寸 noVNC 画布，支持连接状态、重连、全屏和停止。

Browser 页面从现有 IndexedDB 读取设备身份和配对 daemon，不在 URL、localStorage 或页面
文本中保存 access token。页面刷新和 direct/relay 重连不会关闭后端 session。

## 验收矩阵

1. 单元：URL、viewport、manifest、归档 traversal、状态转换和 active session 上限。
2. 模块：fake runtime 下 create/list/connect/close，client detach 后 session 仍为 RUNNING。
3. direct 协议：合法 Bearer 可创建并 attach；无 token、错误 token、错误 ID 被拒绝。
4. relay 协议：Browser route 携带正确 ID，RFB frame opaque 往返，relay 无 browser 持久状态。
5. 前端：URL 创建、列表重开、停止、Browser path 状态与 noVNC 配置。
6. 归档：两个架构资产名称、manifest/hash、权限、禁止项和许可证完整。
7. 代表性 E2E：真实 Xtigervnc/Openbox/Chromium 启动，noVNC 收到非空 framebuffer，键鼠输入
   能改变页面；若当前环境缺少 runtime/Chromium，则标为 `blocked`，不能用 mock 冒充通过。

## 回滚与停止条件

daemon API 在 runtime 缺失、下载失败或 Chromium 不可用时返回稳定的 `503`，不影响 terminal、
file offer 或 relay 其他路由。前端只关闭 Browser 对话框即可退回现有 workspace。

若实现需要公开 RFB TCP 端口、使用 `--no-sandbox`、把 browser 状态放入 relay、下载未经 hash
固定的 latest 资产，或 daemon 重启会把存活 Browser Session 错误删除，则停止推进并重新评审。
