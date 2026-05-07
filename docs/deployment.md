# 公网部署方案

本文给出 termd / termrelay / termctl / Web MVP 的最小公网部署方式。核心原则只有一条：**relay 仍然是不可信的 dumb pipe，公开入口只暴露转发面，不暴露本地管理面。**

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

- `termrelay` 可以是公网边缘服务，但它本身仍然只做转发和路由。
- `termd` 仍建议只监听 loopback 或私网管理网段，`/local/pairing-token` 不应直接暴露到公网。
- 浏览器和 `termctl` 连接 relay 时，使用同一条 client URL。

## 端口与路径

| 组件 | 推荐绑定 | 对外暴露 | 说明 |
| --- | --- | --- | --- |
| `termd` | `127.0.0.1:8765` | 不直接暴露 | 提供 `/healthz`、`/ws`、`/local/pairing-token` |
| `termrelay` | `127.0.0.1:8080` | 通过反向代理暴露到 443 | 提供 `/healthz`、`/ws/{server_id}/daemon`、`/ws/{server_id}/daemon-mux`、`/ws/{server_id}/client` |
| 反向代理 | `0.0.0.0:443` | 是 | 负责 TLS 终止、WebSocket upgrade、日志脱敏 |

公网 client URL 形态如下：

```text
wss://relay.example/ws/{server_id}/client?relay_token=...
```

daemon outbound connector 形态如下：

```text
wss://relay.example/ws/{server_id}/daemon-mux?relay_token=...
```

`relay_token` 是 transport 凭证，不是设备身份，也不是 controller/viewer 控制权。

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

    location /ws/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
```

要点：

- access log 里不要记录 `$request_uri`，否则 `relay_token` 会出现在日志里。
- WebSocket upgrade 必须保留 `Upgrade` / `Connection` 头。
- 如果反向代理做了额外的 rewrite，不要改写 `/ws/{server_id}/...` 结构。

## pairing 边界

- `termd /local/pairing-token` 只适合 loopback 或私网管理面，不要通过公网反代公开。
- `termctl pair` 和 Web MVP 只消费 token，不负责签发 token。
- 如果公网 relay 开启了 `--auth-token`，浏览器和 `termctl` 都需要把 `relay_token` 带在 relay URL query string 中。
- 由于浏览器 WebSocket 不能自由设置自定义 header，`relay_token` 的 query 形式是当前实现的 transport 约束，不应把它当作用户认证方案。

## Health check

- `termd`：`GET /healthz`，返回 `status`、`protocol_version`、`server_id`。
- `termrelay`：`GET /healthz`，返回 `status` 和房间数。
- 反向代理可以公开 relay 的 health check，但 `termd` 的 health check 仍建议留在内网。

## 最小部署命令

```bash
cargo run -p termrelay -- --listen 127.0.0.1:8080 --auth-token "$RELAY_TOKEN"
cargo run -p termd -- --relay wss://relay.example:443 --relay-auth-token "$RELAY_TOKEN"
```

客户端通过同一个 relay 入口访问：

```bash
cargo run -p termctl -- pair --token "$PAIRING_TOKEN" --url "wss://relay.example:443/ws/${SERVER_ID}/client?relay_token=${RELAY_TOKEN}"
```

Web MVP 也使用同样的 relay client URL。

## 运维检查

1. 确认 `relay.example:443` 返回 `healthz`。
2. 确认 `termd` 只在内网/loopback 暴露 `8765`。
3. 确认 `relay_token` 不出现在 access log、proxy error log 或监控事件里。
4. 确认 `/local/pairing-token` 不能从公网访问。
5. 确认 `wss://relay.example/ws/{server_id}/client` 可以完成 pair / new / list。

## 一键安装脚本

release 资产和 GHCR 镜像都由同一个 tag 驱动。发布流水线会把 `scripts/install-*.sh` 渲染成带默认仓库和默认版本的 release 资产，所以常规安装命令不需要再传 `TERMD_GITHUB_REPO` 或 `TERMD_VERSION`。

直接运行仓库里的源码脚本时，它仍然是通用模板，需要通过 `TERMD_GITHUB_REPO=owner/repo` 指定仓库；`TERMD_VERSION` 只保留为高级覆盖项，不作为一键安装的默认入口。

### `termctl`

```bash
curl -fsSL https://github.com/OWNER/REPO/releases/latest/download/install-termctl.sh | sudo bash
```

```bash
wget -qO- https://github.com/OWNER/REPO/releases/latest/download/install-termctl.sh | sudo bash
```

`termctl` 的脚本只安装二进制到 `/usr/local/bin/termctl`，不注册 systemd 服务。

### `termd`

```bash
curl -fsSL https://github.com/OWNER/REPO/releases/latest/download/install-termd.sh | sudo bash
```

```bash
wget -qO- https://github.com/OWNER/REPO/releases/latest/download/install-termd.sh | sudo bash
```

`termd` 脚本会安装二进制、创建 `termd.service`、写入 `/etc/termd/termd.env`（如不存在）并启用服务。默认只监听 `127.0.0.1:8765`，relay 和 TLS 通过 env 文件可选配置。服务启动后，脚本会在当前终端打印一个短期一次性 pairing token 和 `termctl pair` 示例；token 不会写入配置文件。

如果要把内嵌 Web 也一起打开，把 `/etc/termd/termd.env` 里的 `TERMD_WEB_ENABLED=1` 打开即可；脚本会自动追加 `--web`。

### `termrelay`

```bash
curl -fsSL https://github.com/OWNER/REPO/releases/latest/download/install-termrelay.sh | sudo bash
```

```bash
wget -qO- https://github.com/OWNER/REPO/releases/latest/download/install-termrelay.sh | sudo bash
```

`termrelay` 脚本会安装二进制、创建 `termrelay.service`、写入 `/etc/termd/termrelay.env`（如不存在）并启用服务。默认只监听 `127.0.0.1:8080`，公开入口仍建议走反向代理。

如果需要嵌入 Web UI，把 `/etc/termd/termrelay.env` 里的 `TERMRELAY_WEB_ENABLED=1` 打开即可；脚本会自动追加 `--web`。

## GitHub Release 与 GHCR

- tag 采用纯版本号，例如 `0.1.2`。
- tag 推送后，GitHub Actions 会：
  - 运行 workspace 测试，确认 release tag 与 `Cargo.toml` 版本一致。
  - 构建 `termd`、`termrelay`、`termctl` 的 Linux amd64 release tarball。二进制使用 `x86_64-unknown-linux-musl` 静态链接，并在打包前先构建 `termui/frontend` 的静态资源，确保 `termd` 和 `termrelay` 的内嵌 Web 可用。
  - 生成 `checksums.txt` 和带默认仓库/版本的安装脚本，并上传到 GitHub Release。
  - 推送 `ghcr.io/<owner>/termd:<tag>`、`ghcr.io/<owner>/termrelay:<tag>`、`ghcr.io/<owner>/termctl:<tag>` 镜像。
  - 这些镜像使用 `scratch` 运行层；`termd` 和 `termrelay` 同样会内嵌 Web 静态资源。

## `termrelay` docker-compose

`termrelay` 还提供一个容器化部署方式，文件在 [deploy/termrelay/docker-compose.yml](../deploy/termrelay/docker-compose.yml)。使用步骤：

```bash
cd deploy/termrelay
cp .env.example .env
docker compose up -d
```

`.env` 里至少要填写 `TERMRELAY_IMAGE`、`TERMRELAY_DOMAIN`，可选填写 `TERMRELAY_AUTH_TOKEN`。compose 通过 Caddy 终止 TLS，再反向代理到 `termrelay:8080`。
