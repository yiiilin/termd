# 公网部署方案

本文给出 termd / termrelay / termctl / Web MVP 的最小公网部署方式。核心原则只有一条：**relay 是可信 admission/routing 层，daemon connector 必须已注册，client 认证只接受 daemon 签名的 pair/device/access credential，session/PTY 状态仍只在 daemon。** TLS 在反向代理终止后，relay 可以看到并转发明文 WebSocket/HTTP 应用流量；0.7 不存在运行时 E2EE。pairing、auth 和 session 权限仍由 daemon 最终校验。

## 推荐拓扑

```text
Internet
  |
  | 443 / wss
  v
Reverse Proxy (TLS termination + access log control)
  |
  +--> termrelay 127.0.0.1:8080
          ^
          | outbound daemon connector: /ws
          +-- termd 127.0.0.1:8765 (仅 daemon 主机 loopback/私网管理面)
```

- `termrelay` 可以是公网边缘服务，但它只做 admission、转发和路由，不持有 session/PTY 状态。
- trusted relay 不是 WebSocket 应用流量的保密边界；应按可接触终端与控制面明文的可信组件部署和审计。
- `termd` 仍建议只监听 loopback 或私网管理网段，`/local/pairing-token` 不应直接暴露到公网。
- daemon connector 继续使用 `/ws` 注册控制/data pipe；浏览器 workspace 使用 `/ws/metadata` 和 `/ws/terminal`，relay 从 WebSocket subprotocol 中校验 access token 并路由到对应 daemon。

## 端口与路径

| 组件 | 推荐绑定 | 对外暴露 | 说明 |
| --- | --- | --- | --- |
| `termd` | `127.0.0.1:8765` | 不直接暴露 | 提供 `/healthz`、认证 API、`/ws/metadata`、`/ws/terminal` 和 JSON control/file API |
| `termrelay` | `127.0.0.1:8080` | 通过反向代理暴露到 443 | 提供相同的公开 Web/API 路径，并把已认证流量路由到 daemon |
| 反向代理 | `0.0.0.0:443` | 是 | 负责 TLS 终止、WebSocket upgrade、日志脱敏 |

daemon connector 与浏览器 workspace 使用不同的 WebSocket 路径：

```text
wss://relay.example/ws
wss://relay.example/ws/metadata
wss://relay.example/ws/terminal
```

`server_id` 不出现在 URL 或 access log。daemon 会在 connector 的 `route_hello.admission` 里提交 daemon token；浏览器在首次 pairing 后持久保存 device certificate，通过 challenge-response 换取五分钟 access token，并以 `Sec-WebSocket-Protocol: termd.v0.7, <token>` 打开两条 workspace socket。relay 通过 token 的 `kid` 和 daemon registry public key 离线验签。

## TLS 与反向代理

推荐把 TLS 终止放在反向代理层，而不是直接暴露 `termrelay` 的 TLS 证书私钥。

### Nginx / OpenResty 示例

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ""      close;
}

log_format relay_access '"$remote_addr" "$request_method $uri $server_protocol" '
                        '"$status" "$body_bytes_sent" "$http_user_agent"';

server {
    listen 443 ssl http2;
    server_name relay.example;

    ssl_certificate     /etc/letsencrypt/live/relay.example/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/relay.example/privkey.pem;

    access_log /var/log/nginx/relay.access.log relay_access;

    location /healthz {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
    }

    # daemon connector 保持使用精确的 /ws 入口。
    location = /ws {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    # v0.7 workspace sockets: 只开放两个精确路径。
    location = /ws/metadata {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    location = /ws/terminal {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    location /api/control/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    # pairing、challenge、access-token 与 device-certificate migration。
    location /api/auth/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
    }

    # 该路径只用于 daemon token + daemon public key 注册。
    # setup token 在 header 中，反向代理不要记录 headers/body。
    location /api/relay/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
    }

    # v0.7 文件上传、下载路径。
    location /api/files/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
```

要点：

- 不要把 setup token、daemon token、pair ticket、device certificate 或 access token 写进 URL、argv 或反向代理日志。
- 0.7 不接受 `relay_token` 或任何其他 query credential；反向代理不得为旧 query 参数提供兼容 rewrite。
- WebSocket upgrade 必须保留 `Upgrade` / `Connection` 头。
- `/api/auth/*`、`/api/control/*` 和 `/api/files/*` 都必须代理到 relay；不要在反向代理日志中记录它们的 authorization header 或 body。
- `/api/control/*` 使用 `Authorization: Bearer <access_token>` 和标准 JSON request/response；relay 仍只做 admission 和转发，daemon 继续校验权限。
- 如果反向代理做了额外 rewrite，必须同时保留 daemon connector 的精确 `/ws` 与 workspace 的 `/ws/metadata`、`/ws/terminal`。

## token 与 pairing 边界

- `termd /local/pairing-token` 只适合 loopback 或私网管理面，不要通过公网反代公开。
- `termctl pair` 和 Web MVP 只消费 daemon 签发的 pair ticket，不负责签发 credential。
- relay setup token 是 daemon 注册凭证，安装 relay 时生成；只在 `termd` 注册到 relay 时使用，不持久化到浏览器或 pairing invite。
- daemon token 由 `termd` 安装脚本自动生成并保存为 `/etc/termd/termd_daemon_token`；relay registry 只持久化它的 hash。
- pair ticket 只用于首次 pairing；device certificate 绑定设备 public key 并持久保存；access token 五分钟过期，客户端提前一分钟刷新。
- 旧设备只能通过受限 `POST /api/auth/device-certificate/migrate` 换取 device certificate；不存在 relay 侧 pair/device/token 同步注册。
- relay registry 保存 daemon token hash 和 daemon public key，不保存 pair ticket、device certificate、access token 或设备私钥。

daemon registry JSON 示例：

```json
{
  "daemons": [
    {
      "server_id": "00000000-0000-0000-0000-000000000001",
      "token_hash": "sha256:...",
      "daemon_public_key": "ed25519-v1:..."
    }
  ]
}
```

## HTTP control 与文件传输

0.7 默认开放当前 JSON auth/control 路由和六个文件传输路由，不需要兼容开关。除 upload chunk request 与 download byte body 外，所有 application HTTP request/response 都使用 JSON；所有错误（包括 404/405）固定为 `{"error":{"code":"...","message":"...","retryable":false}}`。

文件传输使用 `/api/files/uploads`、`/api/files/uploads/{id}/chunks`、`/api/files/uploads/{id}/commit|abort`、`/api/files/downloads` 和 `/api/files/downloads/{id}`。旧 `/api/files/upload/*`、download prepare/chunk、session token、session scope 和 HTTP E2EE 路径不再支持。

## 0.7 升级顺序

1. 先升级 `termrelay` 并确认 `GET /healthz` 正常。0.6 registry entry 缺少 daemon public key，不能仅凭旧 token hash 完成 0.7 client 验签。
2. 把 relay setup token 安全复制到 daemon 主机的 root-only 临时文件，再升级本地 `termd` 并传入 `--relay-setup-token-file`，让 installer 重新注册 daemon token hash 与 public key。installer 检测到 `SUPERVISOR_VERSION=2026-07-12-dual-ws` 与旧值不一致时，会提示旧 session 将丢失；确认后停止 daemon、清理旧 session runtime、写入新 compatibility id 并重启。
3. 重新加载 Web UI；既有设备通过受限 migration endpoint 换取 device certificate，随后使用 access token。

不要只升级 daemon 或只升级 Web UI。升级不会删除 daemon identity 和 pairing 设备状态，但 0.6 supervisor session 不能跨本次兼容版本保留。

## Health check

- `termd`：`GET /healthz`，返回 `status`、`protocol_version`、`server_id`、`daemon_public_key`。
- `termrelay`：`GET /healthz`，返回 `status`、房间数和 `trusted_admission`。
- 反向代理可以公开 relay 的 health check，但 `termd` 的 health check 仍建议留在内网。

## 最小部署命令

推荐直接用安装脚本完成 relay setup token、daemon token 和 registry 注册，避免手工生成 `server_id` 映射：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh \
  | sudo bash -s -- --web --listen 127.0.0.1:8080

# 在 daemon 主机上，把通过安全通道取得的 setup token 放进 root-only 临时文件。
sudo install -m 0600 /secure/source/termrelay_setup_token /run/termd-relay-setup-token
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --relay wss://relay.example:443 \
      --relay-setup-token-file /run/termd-relay-setup-token
sudo rm -f /run/termd-relay-setup-token
```

安装脚本会生成 relay setup token 和 daemon token，并调用 relay 注册 API 把 daemon token hash 与 daemon public key 写入 registry。先通过安全通道把 relay 主机的 setup token 复制到 daemon 主机的 root-only 临时文件，再把该文件路径传给 `--relay-setup-token-file`；注册完成后删除 daemon 主机上的临时副本。公网部署不要把任何凭据放进 argv、URL 或日志。

生成一份可在 daemon Web 和 relay Web 里直接使用的单行邀请码。邀请码携带 daemon 标识、public key 和短期 pair ticket；Web 默认使用当前页面的 HTTP(S) base，普通使用者不需要查看或拼接 `server_id`：

```bash
PAIR_INVITE_FILE="$(mktemp)"
chmod 0600 "$PAIR_INVITE_FILE"
termd pair --qr | tail -n1 >"$PAIR_INVITE_FILE"
```

客户端通过同一个 relay 入口访问：

```bash
termctl pair --payload-file "$PAIR_INVITE_FILE" --url "https://relay.example"
rm -f "$PAIR_INVITE_FILE"
```

Web MVP 打开 daemon 页面或 relay 页面后都粘贴同一段 `termd-pair:v2:...` 邀请码。页面默认使用当前地址；需要其他地址时，使用 Web 的高级地址设置手动覆盖。pair ticket 由 daemon 最终验证，pairing 成功后返回持久 device certificate，后续请求不再复用 pair ticket。

## 运维检查

1. 确认 `relay.example:443` 返回 `healthz`。
2. 确认 `termd` 只在内网/loopback 暴露 `8765`。
3. 确认 setup token、daemon token、pair ticket、device certificate 和 access token 不出现在 access log、proxy error log 或监控事件里。
4. 确认 `/local/pairing-token` 不能从公网访问。
5. 确认 `POST /api/auth/pair` 可以完成首次配对，随后 workspace 稳定保持 `/ws/metadata` 与 `/ws/terminal` 两条连接；relay 通过 access token 的 `kid=server_id` 和 daemon registry public key 验签路由。

## 一键安装脚本

release 资产和 GHCR 镜像都由同一个 tag 驱动。发布流水线会把 `scripts/install-*.sh` 渲染成带默认仓库和默认版本的 release 资产，所以常规安装命令不需要再传 `TERMD_GITHUB_REPO` 或 `TERMD_VERSION`。

直接运行仓库里的源码脚本时，它仍然是通用模板，需要通过 `TERMD_GITHUB_REPO=owner/repo` 指定仓库；`TERMD_VERSION` 只保留为高级覆盖项，不作为一键安装的默认入口。

### `termctl`

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termctl.sh | sudo bash
```

`termctl` 的脚本只安装二进制到 `/usr/local/bin/termctl`，不注册 systemd 服务。

### `termd`

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh | sudo bash
```

`termd` 脚本会安装二进制、创建 `termd.service`、写入 `/etc/termd/termd.env`（如不存在）并启用服务。默认只监听 `127.0.0.1:8765`，relay 和 TLS 通过 env 文件可选配置。服务启动后，脚本会在当前终端显示一次短期敏感的 `termd-pair:v2` 邀请码，并给出不含凭据的 `termctl pair --payload-file` 命令模板；不会重复打印裸 token，也不会把 invite 拼进 argv、URL 或配置文件。邀请过期或用过后可在 daemon 主机上运行 `termd pair --qr` 重新签发。

`termd.service` 使用 `KillMode=process`，这样 `systemctl restart termd` 只会重启 daemon 主进程，不会把每个 session 的 supervisor 子进程一起清掉；显式 close 仍然由 daemon 协议路径负责。

回滚到不认识私有 `session_ownership` ledger 的旧 daemon 前，必须确认没有 create 或 cleanup 正在持久收敛。installer 会在替换二进制前执行同等的只读检查；手工回滚可先运行：

```bash
sudo sqlite3 -readonly /var/lib/termd/daemon-state.sqlite \
  "SELECT phase, COUNT(*) FROM session_ownership WHERE phase IN ('preparing','cleaning') GROUP BY phase;"
```

查询无结果才允许停止当前 daemon 并回滚。数据库没有 `session_ownership` 表表示尚未使用该 ledger，也满足 precheck；查询返回任何行时应保持当前 daemon 运行并等待收敛，不能删除 ledger 行、socket 或 supervisor 进程来绕过检查。

如果要把内嵌 Web 也一起打开，把 `/etc/termd/termd.env` 里的 `TERMD_WEB_ENABLED=1` 打开即可；脚本会自动追加 `--web`。

### `termrelay`

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash
```

`termrelay` 脚本会安装二进制、创建 `termrelay.service`、写入 `/etc/termd/termrelay.env`（如不存在）并启用服务。默认只监听 `127.0.0.1:8080`，并自动创建 setup token 文件与 daemon registry；公开入口仍建议走反向代理。

`termrelay.service` 也保留了 `KillMode=process`，只是为了让 systemd 停止动作保持和 daemon 一致；它本身不承担 session supervisor 生命周期。

如果需要嵌入 Web UI，把 `/etc/termd/termrelay.env` 里的 `TERMRELAY_WEB_ENABLED=1` 打开即可；脚本会自动追加 `--web`。

0.7 的 auth、control 和六个 file routes 都默认启用；installer 不再接受 `--http-tunnel` 或长期 relay transport token 参数。

## GitHub Release 与 GHCR

- tag 采用纯版本号，例如 `0.1.2`。
- 本机从源码更新正在运行的 daemon 时，优先使用 `scripts/update-local-termd.sh`：

  ```bash
  sudo scripts/update-local-termd.sh --workspace-tests
  ```

  脚本会先运行格式检查、Rust 测试和 release 编译，再记录当前 `termd.service`
  的 `KillMode`、主进程、session supervisor PID、SQLite session 计数和 health
  状态。supervisor compatibility 未变化时，它只替换 `/usr/local/bin/termd` 并
  校验 supervisor PID/session 集合不变；版本变化到 `2026-07-12-dual-ws` 时，
  会先停止 daemon、终止旧 supervisor、清空不兼容 session，再写入新版本并重启。
- 常规发版仍使用 `scripts/prepare-release.sh <version>`。发版脚本会同步版本号、
  生成 release notes、运行安装脚本回归、Rust/workspace 验证、Web typecheck/test/build
  和 release 编译，然后在隔离 worktree 中创建 release commit 和带用户可见说明的
  annotated tag。无论是否传 `--push` 或 `--allow-dirty`，脚本返回时都不会推进本地
  `main`，也不会修改 caller 的 index/worktree；传 `--push` 只会用精确 commit/tag OID
  原子更新远端 `main` 和 tag，并触发 GitHub Actions。
- 脚本输出会先提示处理无关 caller 改动，再给出经过回归测试的本地完成命令；命令使用
  精确 release notes 路径和 release commit OID：

  ```bash
  git add -- 'docs/releases/<version>.md'
  git merge --ff-only <release-commit-oid>
  ```

  第一条命令只暂存该版本的 release notes，不会纳入其他文件；第二条命令只有在当前
  `main` 仍可快进到精确 release commit 时才成功。clean caller 回归会原样执行这两条
  输出命令，并确认完成后本地 `main` 指向 release commit、status 为空且 index 没有残留
  staged diff。使用 `--allow-dirty` 时，应先处理脚本原样保留的其他 caller 改动，再执行
  输出中的同一流程。
- tag 推送后，GitHub Actions 会：
  - 运行 workspace 测试，确认 release tag 与 `Cargo.toml` 版本一致。
  - 构建 `termd`、`termrelay`、`termctl` 的 Linux amd64 release tarball。二进制使用 `x86_64-unknown-linux-musl` 静态链接，并在打包前先构建 `termui/frontend` 的静态资源，确保 `termd` 和 `termrelay` 的内嵌 Web 可用。
  - 当前不发布 Linux arm64 tarball；arm64 安装脚本会跳过 release asset 并从源码构建，不承诺不存在的 arm64 资产。
  - 生成 `checksums.txt` 和带默认仓库/版本的安装脚本，并上传到 GitHub Release。
  - 推送 `ghcr.io/<owner>/termd:<tag>`、`ghcr.io/<owner>/termrelay:<tag>`、`ghcr.io/<owner>/termctl:<tag>` 镜像。
  - 这些镜像使用 `scratch` 运行层；`termd` 和 `termrelay` 同样会内嵌 Web 静态资源。

## `termrelay` docker-compose

`termrelay` 还提供一个容器化部署方式，文件在 [deploy/termrelay/docker-compose.yml](../deploy/termrelay/docker-compose.yml)。使用步骤：

```bash
cd deploy/termrelay
cp .env.example .env
```

`.env` 里至少要填写 `TERMRELAY_IMAGE`、`TERMRELAY_DOMAIN` 和 `TERMRELAY_SETUP_TOKEN_FILE`。这个 compose 文件面向公网 Caddy 部署，启动时会要求 setup token secret 文件路径非空；compose 通过 Caddy 终止 TLS，再反向代理到 `termrelay:8080`。

先生成 relay setup token secret 文件，再把 `.env` 里的路径指向它，最后启动 compose。daemon registry 存在 compose 的 `termrelay_state` volume 内，由注册 API 维护：

```bash
sudo install -d -m 0755 /etc/termd
openssl rand -hex 32 | sudo tee /etc/termd/termrelay_setup_token >/dev/null
sudo chown 10001:10001 /etc/termd/termrelay_setup_token
sudo chmod 400 /etc/termd/termrelay_setup_token
docker compose up -d
```

compose 会把 setup token 文件作为 Docker secret 挂载到 `/run/secrets/termrelay_setup_token`；release 镜像以 UID/GID `10001` 运行，因此 host 上的 secret 文件需要对 `10001:10001` 可读，同时不能 world-readable。token 文件末尾换行会被忽略，空文件或全空白内容会导致启动失败。不要把真实 token 写进 `.env` 或 compose command，这样 `docker compose config` 和 Docker inspect metadata 只会包含 secret 文件路径，不会展开 token 明文。secret 文件不要放在仓库目录内，也不要提交到 git。

0.7 不再使用 `--http-tunnel` 开关；compose 必须代理 `/ws`、`/ws/metadata`、`/ws/terminal`、`/api/auth/*`、`/api/control/*` 和 `/api/files/*`。如果改用自定义 Caddyfile 或额外启用 access log，必须避免 setup token、daemon token、pair ticket、device certificate 和 access token 进入 stdout、文件日志或集中日志系统。

仅本机开发或一次性 smoke 才可以跳过 trusted daemon admission，并且不要复用上面的公网 Caddy compose。可以直接在 loopback 上运行：

```bash
cargo run -p termrelay -- --listen 127.0.0.1:8080 --allow-open-relay
```

如果使用容器做本机无认证检查，也应只绑定到 loopback：

```bash
docker run --rm -p 127.0.0.1:8080:8080 ghcr.io/yiiilin/termrelay:0.7.0 --listen 0.0.0.0:8080 --allow-open-relay
```
