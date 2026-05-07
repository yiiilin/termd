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

`relay_token` 是 transport 凭证，不是设备身份，也不是 controller/viewer 权限。

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

