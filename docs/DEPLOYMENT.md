# Deployment

hls-proxy is a single binary that listens on a port. Everything below is about
getting it a domain, TLS, and sensible limits.

## Configuration

All configuration is environment variables. A `.env` file in the working
directory is loaded if present.

| Variable | Default | Meaning |
|---|---|---|
| `BASE_URL` | *(request `Host`)* | Public origin the rewritten URLs point at. |
| `BIND` | `0.0.0.0` | Address to bind. |
| `PORT` | `8080` | Port to listen on. |
| `DEFAULT_EMULATION` | `chrome_137` | Browser profile for upstream requests. |
| `DEFAULT_EMULATION_OS` | `windows` | Platform that profile presents as. |
| `PROXY_URL` | *(none)* | Route upstream requests through an HTTP/HTTPS proxy, e.g. `http://user:pass@host:port`. Fails at startup if malformed. |
| `PROXY_MODE` | `always` | `always` proxies every request; `fallback` goes direct until a host 429s, then proxies just that host (lower latency for live HLS). |
| `SEGMENT_MODE` | `proxy` | `proxy` relays every segment; `auto` serves segments that work without special headers as direct CDN links (only for providers with IP-agnostic segment URLs). |
| `RUST_LOG` | `hls_proxy=info` | Log filter, e.g. `hls_proxy=debug,tower_http=info`. |

### Getting `BASE_URL` right

This is the setting people get wrong, and the failure is confusing: the master
playlist loads fine, then every segment 404s or hits the wrong host.

The proxy has to write absolute URLs into the playlists it rewrites, so it needs
to know its own public address. If `BASE_URL` is unset it derives one from each
request's `Host` header, honouring `X-Forwarded-Proto`. That works locally and
behind a correctly configured reverse proxy.

Set it explicitly when:

- you terminate TLS upstream and the proxy cannot tell it is being served over
  `https` (or make sure `X-Forwarded-Proto` is forwarded);
- the internal `Host` differs from the public domain;
- requests reach the proxy through more than one hostname and you want one
  canonical form.

```bash
BASE_URL=https://hls-proxy.example.com
```

Omit any trailing slash — it is stripped anyway.

## systemd

```ini
# /etc/systemd/system/hls-proxy.service
[Unit]
Description=hls-proxy
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/hls-proxy
Environment=BASE_URL=https://hls-proxy.example.com
Environment=BIND=127.0.0.1
Environment=PORT=8080
Environment=RUST_LOG=hls_proxy=info
Restart=always
RestartSec=2

# Nothing to write to disk, so lock it down.
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=512M

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now hls-proxy
journalctl -u hls-proxy -f
```

Binding to `127.0.0.1` keeps it unreachable except through the reverse proxy —
which matters, because the proxy has no authentication of its own.

## Docker

A [Dockerfile](../Dockerfile) is included. It builds BoringSSL in a Rust builder
image and ships only the binary in a slim runtime image.

```bash
docker build -t hls-proxy .

docker run -d --name hls-proxy \
  -p 8080:8080 \
  -e BASE_URL=https://hls-proxy.example.com \
  --restart unless-stopped \
  hls-proxy
```

The first build compiles BoringSSL and takes several minutes; later builds reuse
the cached layers as long as `Cargo.toml`/`Cargo.lock` are unchanged.

```yaml
# compose.yaml
services:
  hls-proxy:
    build: .
    ports:
      - "8080:8080"
    environment:
      BASE_URL: https://hls-proxy.example.com
      RUST_LOG: hls_proxy=info
    restart: unless-stopped
```

## Reverse proxies

Two things matter for any reverse proxy in front of hls-proxy:

1. **Forward `X-Forwarded-Proto`**, or set `BASE_URL`, so rewritten URLs use
   `https`.
2. **Do not buffer responses.** Segments are streamed; buffering them adds
   latency and memory use for no benefit.

### Caddy

Caddy gets TLS and the forwarding headers right by default:

```
hls-proxy.example.com {
    reverse_proxy 127.0.0.1:8080 {
        flush_interval -1
    }
}
```

`flush_interval -1` disables response buffering.

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name hls-proxy.example.com;

    ssl_certificate     /etc/letsencrypt/live/hls-proxy.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/hls-proxy.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;

        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;

        # Stream segments instead of buffering them.
        proxy_buffering off;
        proxy_request_buffering off;

        # Long enough for slow origins.
        proxy_read_timeout 300s;
        proxy_send_timeout 300s;
    }
}
```

## Hardening

**hls-proxy ships with no authentication.** Anyone who can reach it can encode a
payload for any URL and use your server as an open proxy, at your bandwidth
expense. Treat exposing it publicly as a deliberate decision, not a default.

Options, roughly in order of effort:

**Keep it private.** Bind to `127.0.0.1` and reach it only from an app on the
same host, or restrict it to a private network or VPN.

**Authenticate at the reverse proxy.** The simplest real control. With Caddy:

```
hls-proxy.example.com {
    basic_auth {
        viewer $2a$14$...   # caddy hash-password
    }
    reverse_proxy 127.0.0.1:8080
}
```

Note that browser players cannot easily attach credentials to segment requests,
so basic auth suits server-to-server use better than in-browser playback.

**Restrict by referrer or origin** at the reverse proxy, if your player runs on
a known site. Weak on its own — headers are trivially forged — but it deters
casual reuse.

**Rate-limit.** Segment traffic is bursty by nature, so set limits generously or
you will break playback. Per-IP connection limits tend to work better than
request-rate limits.

**Limit egress.** The SSRF guard rejects reserved *IP literals* — loopback,
private, link-local (including cloud metadata at `169.254.169.254`), carrier-NAT
and reserved ranges, in both IPv4 and IPv6 including IPv4-mapped forms — and it
re-checks every redirect hop. What it cannot catch is a hostname that resolves to
an internal address, because resolution happens inside the HTTP client. If the
proxy runs somewhere with access to internal services, restrict its outbound
network at the firewall rather than relying on the guard.

## Scaling

Because tokens are stateless, scaling is ordinary round-robin load balancing —
no sticky sessions, no shared cache, no coordination. Run as many instances as
you need behind any load balancer, and set the same `BASE_URL` on all of them.

All traffic flows through the proxy, so bandwidth, not CPU, is almost always the
limit. A CDN in front helps a great deal: segment URLs are stable and immutable,
so they cache well. Playlists are returned with `no-cache` and will correctly
not be cached, which is what you want for live streams.

## Monitoring

`GET /` is a dependency-free health check — it answers without touching any
upstream, so it reports on the proxy alone.

```bash
curl -fsS http://127.0.0.1:8080/ > /dev/null || echo "down"
```

Set `RUST_LOG=hls_proxy=debug,tower_http=debug` to log every request. Upstream
failures are logged with the target URL at `warn`, which is usually the fastest
way to find a stream whose headers have gone stale.
