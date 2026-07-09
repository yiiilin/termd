# 公网部署方案

本文给出 termd / termrelay / termctl / Web MVP 的最小公网部署方式。核心原则只有一条：**relay 是可信 admission/routing 层，公开入口只接受已注册 daemon 和短期 client admission，session/PTY 状态仍只在 daemon。**

## 推荐拓扑

```text
Internet
  |
  | 443 / wss
  v
Reverse Proxy (TLS termination + access log control)
  |
  +--> termrelay 127.0.0.1:8080
  +--> termd     127.0.0.1:8765   (仅私网/loopback 管理面)
```

- `termrelay` 可以是公网边缘服务，但它只做 admission、转发和路由，不持有 session/PTY 状态。
- `termd` 仍建议只监听 loopback 或私网管理网段，`/local/pairing-token` 不应直接暴露到公网。
- 浏览器和 `termctl` 连接 relay 时，使用同一条 `/ws` URL；daemon 路由由连接后的 `route_hello.server_id` 决定。

## 端口与路径

| 组件 | 推荐绑定 | 对外暴露 | 说明 |
| --- | --- | --- | --- |
| `termd` | `127.0.0.1:8765` | 不直接暴露 | 提供 `/healthz`、`/ws`、`/local/pairing-token` |
| `termrelay` | `127.0.0.1:8080` | 通过反向代理暴露到 443 | 提供 `/healthz`、`/ws`；首个 WebSocket frame 必须是 `route_hello`；HTTP 文件 tunnel 默认关闭 |
| 反向代理 | `0.0.0.0:443` | 是 | 负责 TLS 终止、WebSocket upgrade、日志脱敏 |

公网 client 和 daemon outbound connector 使用同一个 WebSocket 入口：

```text
wss://relay.example/ws
```

`server_id` 不再出现在 URL path 中，而是在连接建立后的 `route_hello` 明文前置握手里声明。daemon 会在 `route_hello.admission` 里提交 daemon token，relay 通过 daemon registry 决定是否允许该 daemon 进入对应 `server_id` 房间。浏览器和 `termctl` 使用短期 PairTicket admission，不需要长期 `relay_token` query。

## TLS 与反向代理

推荐把 TLS 终止放在反向代理层，而不是直接暴露 `termrelay` 的 TLS 证书私钥。

### Nginx 示例

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

    location = /ws {
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

    # daemon 注册、pair ticket 注册和 device admission 注册使用该路径。
    # 这些请求携带短期或长期 admission 材料，反向代理不要记录 headers/body。
    location /api/relay/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
    }

    # 只有启用 termrelay --http-tunnel 后才需要这些文件传输兼容路径。
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

- 不要把 setup token、daemon token 或旧 `relay_token` 写进 URL、argv 或反向代理日志。
- WebSocket upgrade 必须保留 `Upgrade` / `Connection` 头。
- `/api/control/*` 是默认启用的 HTTP control tunnel；relay 仍只做 route admission 和转发，daemon 继续校验 bearer/session 权限。
- 如果反向代理做了额外的 rewrite，最终仍必须把公开入口收敛到 relay 的 `/ws`。

## token 与 pairing 边界

- `termd /local/pairing-token` 只适合 loopback 或私网管理面，不要通过公网反代公开。
- `termctl pair` 和 Web MVP 只消费 token，不负责签发 token。
- relay setup token 是 daemon 注册凭证，安装 relay 时生成；只在 `termd` 注册到 relay 时使用，不持久化到浏览器或 pairing invite。
- daemon token 由 `termd` 安装脚本自动生成并保存为 `/etc/termd/termd_daemon_token`；relay registry 只持久化它的 hash。
- 旧 `--auth-token-file` / `relay_token` query 仍可用于迁移兼容；新 trusted relay 默认不要求浏览器或 `termctl` 携带长期 query token。
- client 侧 `route_hello.admission` 只是在 trusted relay 上要求“带 admission 外壳”后才分配 daemon data pipe；pairing token、device signature、bearer 和 session scope 的最终校验仍全部在 daemon。

daemon registry JSON 示例：

```json
{
  "daemons": [
    {
      "server_id": "00000000-0000-0000-0000-000000000001",
      "token_hash": "sha256:..."
    }
  ]
}
```

## HTTP 文件 tunnel 兼容开关

`termrelay` 默认挂载 `/healthz`、`/ws`、`/api/control/*`、`/api/relay/*` 和可选 Web fallback。`/api/files/upload/init`、`/api/files/upload`、`/api/files/upload/abort`、`/api/files/download` 默认返回非成功状态，并提示需要 `--http-tunnel`。

只有需要旧版浏览器文件上传/下载经 relay 中转时，才显式启用：

```bash
cargo run -p termrelay -- --listen 127.0.0.1:8080 \
  --setup-token-file /etc/termd/termrelay_setup_token \
  --daemon-registry /var/lib/termrelay/daemon-registry.json \
  --http-tunnel
```

启用后 relay 只把 HTTP request/response body 编码为 tunnel frame 转发给 daemon data pipe，不保存文件、不判断 session 权限；实际 bearer、scope token、pairing/auth 仍由 daemon 校验。

## Health check

- `termd`：`GET /healthz`，返回 `status`、`protocol_version`、`server_id`。
- `termrelay`：`GET /healthz`，返回 `status`、房间数和 `trusted_admission`。
- 反向代理可以公开 relay 的 health check，但 `termd` 的 health check 仍建议留在内网。

## 最小部署命令

推荐直接用安装脚本完成 relay setup token、daemon token 和 registry 注册，避免手工生成 `server_id` 映射：

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh \
  | sudo bash -s -- --web --listen 127.0.0.1:8080

curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termd.sh \
  | sudo bash -s -- --relay wss://relay.example:443 \
      --relay-setup-token-file /etc/termd/termrelay_setup_token
```

安装脚本会生成 relay setup token 和 daemon token，并调用 relay 注册 API 把 daemon token hash 写入 registry。公网部署不要把 setup token 或 daemon token 放进 argv；内联 token 参数只保留给本机 smoke/dev。

生成一份可在 daemon Web 和 relay Web 里直接使用的单行邀请码。邀请码只包含 daemon 标识和短期 token；Web 默认使用当前页面的连接地址，普通使用者不需要查看或拼接 `server_id`：

```bash
PAIR_INVITE="$(cargo run -q -p termd -- pair --qr | tail -n1)"
```

客户端通过同一个 relay 入口访问：

```bash
cargo run -p termctl -- pair --payload "$PAIR_INVITE" --url "wss://relay.example/ws"
```

Web MVP 打开 daemon 页面或 relay 页面后都粘贴同一段 `termd-pair:v2:...` 邀请码。页面默认使用当前地址；需要其他地址时，使用 Web 的高级地址设置手动覆盖。relay 做入口 admission 和 daemon 路由；pairing token 仍由 daemon 最终验证。

## 运维检查

1. 确认 `relay.example:443` 返回 `healthz`。
2. 确认 `termd` 只在内网/loopback 暴露 `8765`。
3. 确认 setup token、daemon token 和旧 `relay_token` 不出现在 access log、proxy error log 或监控事件里。
4. 确认 `/local/pairing-token` 不能从公网访问。
5. 确认 `wss://relay.example/ws` 可以完成 pair / new / list，relay 通过 `route_hello.server_id` 和 daemon registry 选择 daemon。

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

`termd` 脚本会安装二进制、创建 `termd.service`、写入 `/etc/termd/termd.env`（如不存在）并启用服务。默认只监听 `127.0.0.1:8765`，relay 和 TLS 通过 env 文件可选配置。服务启动后，脚本会在当前终端打印一份短期一次性 `termd-pair:v2` 邀请码和 `termctl pair --payload` 示例；邀请材料不会写入配置文件，过期或用过后可在 daemon 主机上运行 `termd pair --qr` 重新签发。

`termd.service` 使用 `KillMode=process`，这样 `systemctl restart termd` 只会重启 daemon 主进程，不会把每个 session 的 supervisor 子进程一起清掉；显式 close 仍然由 daemon 协议路径负责。

如果要把内嵌 Web 也一起打开，把 `/etc/termd/termd.env` 里的 `TERMD_WEB_ENABLED=1` 打开即可；脚本会自动追加 `--web`。

### `termrelay`

```bash
curl -fsSL https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash
```

```bash
wget -qO- https://github.com/yiiilin/termd/releases/latest/download/install-termrelay.sh | sudo bash
```

`termrelay` 脚本会安装二进制、创建 `termrelay.service`、写入 `/etc/termd/termrelay.env`（如不存在）并启用服务。默认只监听 `127.0.0.1:8080`，公开入口仍建议走反向代理。可信 relay 生产部署还应在 env 或 systemd override 中配置 daemon registry 文件。

`termrelay.service` 也保留了 `KillMode=process`，只是为了让 systemd 停止动作保持和 daemon 一致；它本身不承担 session supervisor 生命周期。

如果需要嵌入 Web UI，把 `/etc/termd/termrelay.env` 里的 `TERMRELAY_WEB_ENABLED=1` 打开即可；脚本会自动追加 `--web`。

如果需要通过 relay 使用旧版 HTTP 文件上传/下载路径，还需要给 `termrelay` 额外传入 `--http-tunnel`，或在 installer env 中设置 `TERMRELAY_HTTP_TUNNEL=1`。默认关闭时，WebSocket 终端、pairing 和 HTTP control tunnel 仍正常工作。

## GitHub Release 与 GHCR

- tag 采用纯版本号，例如 `0.1.2`。
- 本机从源码更新正在运行的 daemon 时，优先使用 `scripts/update-local-termd.sh`：

  ```bash
  sudo scripts/update-local-termd.sh --workspace-tests
  ```

  脚本会先运行格式检查、Rust 测试和 release 编译，再记录当前 `termd.service`
  的 `KillMode`、主进程、session supervisor PID、SQLite session 计数和 health
  状态。只有 `KillMode=process` 时才会替换 `/usr/local/bin/termd` 并重启
  `termd.service`；重启后会校验 healthz、supervisor PID 集合不变、running
  session 数没有下降。它不会手动删除 SQLite 数据，也不会终止
  `__session-supervisor` 进程。
- 常规发版仍使用 `scripts/prepare-release.sh <version>`。发版脚本会同步版本号、
  生成 release notes、运行安装脚本回归、Rust/workspace 验证、Web typecheck/test/build
  和 release 编译，然后创建带用户可见说明的 annotated tag；传 `--push` 才会推送并触发
  GitHub Actions。
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

该 compose 默认不传 `--http-tunnel`。如果必须保留旧版文件 HTTP tunnel，在 `termrelay.command` 里追加 `--http-tunnel`，同时确认反向代理只暴露预期路径。

随 compose 提供的 Caddyfile 保留了旧 `relay_token` 日志脱敏，用于迁移兼容。如果改用自定义 Caddyfile 或额外启用 access log，也必须避免 setup token、daemon token 或旧 `relay_token` 进入 stdout、文件日志或集中日志系统。

仅本机开发或一次性 smoke 才可以不启用 relay token，并且不要复用上面的公网 Caddy compose。可以直接在 loopback 上运行：

```bash
cargo run -p termrelay -- --listen 127.0.0.1:8080 --allow-open-relay
```

如果使用容器做本机无认证检查，也应只绑定到 loopback：

```bash
docker run --rm -p 127.0.0.1:8080:8080 ghcr.io/yiiilin/termrelay:0.6.4 --listen 0.0.0.0:8080 --allow-open-relay
```
