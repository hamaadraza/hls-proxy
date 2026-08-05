# hls-proxy

[![CI](https://github.com/hamaadraza/hls-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/hamaadraza/hls-proxy/actions/workflows/ci.yml)
[![Release](https://github.com/hamaadraza/hls-proxy/actions/workflows/release.yml/badge.svg)](https://github.com/hamaadraza/hls-proxy/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Play any HLS stream from your own domain — headers, CORS and all.**

Give it a stream URL plus the headers that stream requires, and it hands back an
`.m3u8` that plays anywhere. Every playlist, segment and key is served from your
domain, and the original request headers are reattached automatically all the
way down to the last segment.

```bash
curl "http://localhost:8080/encode?url=https://example.com/master.m3u8&header=Referer:https://example.com/"
```

```json
{
  "url": "http://localhost:8080/proxy/eyJ1cmwiOiJodHRwczovL2V4YW1wbGUu...",
  "payload": "eyJ1cmwiOiJodHRwczovL2V4YW1wbGUu..."
}
```

Drop that `url` into hls.js, VLC, ffmpeg or a `<video>` tag and it just plays.

## The problem it solves

A protected HLS stream usually refuses to play in a browser for three reasons:

1. **It demands headers on every request.** `Referer`, `Origin`, a specific
   `User-Agent` — and not just for the playlist, but for all several thousand
   segments. Browser video players give you no way to attach custom headers to
   segment requests.
2. **CORS blocks it.** The origin never sent `Access-Control-Allow-Origin`, so
   the browser refuses the response even when the request succeeds.
3. **The origin only trusts browsers.** Some reject any client that doesn't
   *look* like one, right down to the TLS handshake.

hls-proxy sits in the middle and handles all three. Your player only ever talks
to your domain, over plain CORS-enabled HTTP, and the proxy does the awkward
part upstream.

## Features

- **Headers propagate automatically.** Set them once; every variant playlist,
  segment, AES key and init segment inherits them.
- **Stateless.** Everything needed to fetch a resource is encoded in its URL, so
  there is no session store, nothing to expire, and you can run any number of
  instances behind a load balancer.
- **Segments stream through.** Video bytes are never buffered in memory, and
  `Range` requests are forwarded so seeking works.
- **Live and VOD.** Live playlists are re-fetched and rewritten on every refresh.
- **Handles real-world playlists.** Master and media playlists, `EXT-X-KEY`,
  `EXT-X-MAP`, `EXT-X-MEDIA`, I-frame streams, low-latency `EXT-X-PART`,
  relative and absolute URLs, inline `data:` keys.
- **Browser-identical requests.** Upstream fetches use a real browser's TLS and
  HTTP/2 fingerprint, so origins that fingerprint clients still serve them.
- **One binary.** No runtime dependencies, no config file required.

## Install

Download a binary from the
[releases page](https://github.com/hamaadraza/hls-proxy/releases):

| Platform | Architecture | Archive |
|---|---|---|
| Linux | x86_64 | `hls-proxy-<tag>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | arm64 | `hls-proxy-<tag>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Apple silicon | `hls-proxy-<tag>-aarch64-apple-darwin.tar.gz` |
| macOS | Intel | `hls-proxy-<tag>-x86_64-apple-darwin.tar.gz` |
| Windows | x86_64 | `hls-proxy-<tag>-x86_64-pc-windows-msvc.zip` |
| Windows | arm64 | `hls-proxy-<tag>-aarch64-pc-windows-msvc.zip` |

Every archive ships a `.sha256` checksum. Linux builds target glibc 2.35
(Ubuntu 22.04) for broad compatibility.

> Archive names use Rust's `arch-vendor-os-abi` target naming. `unknown` is just
> the vendor field for platforms without a single vendor, so
> `x86_64-unknown-linux-gnu` means "64-bit Linux, glibc" — the right choice for
> almost every Linux server. `aarch64` is 64-bit ARM.

Or build it yourself — see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md):

```bash
cargo build --release
```

## Usage

Start the server:

```bash
hls-proxy
```

It listens on `0.0.0.0:8080` and needs no configuration to work locally.

### Build a stream URL

The `/encode` endpoint turns a stream and its headers into a playable URL:

```bash
curl "http://localhost:8080/encode?url=https://example.com/master.m3u8&header=Referer:https://example.com/&header=Origin:https://example.com"
```

Repeat `header` as many times as you need. For longer header sets, POST JSON
instead:

```bash
curl -X POST http://localhost:8080/encode \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://example.com/master.m3u8",
    "headers": {
      "Referer": "https://example.com/",
      "User-Agent": "Mozilla/5.0 ..."
    }
  }'
```

### Play it

```html
<video id="video" controls></video>
<script src="https://cdn.jsdelivr.net/npm/hls.js@latest"></script>
<script>
  const hls = new Hls();
  hls.loadSource("http://localhost:8080/proxy/eyJ1cmwiOi...");
  hls.attachMedia(document.getElementById("video"));
</script>
```

Or from the command line:

```bash
ffplay "http://localhost:8080/proxy/eyJ1cmwiOi..."
ffmpeg -i "http://localhost:8080/proxy/eyJ1cmwiOi..." -c copy out.mp4
```

Full endpoint and payload reference: **[docs/API.md](docs/API.md)**.

## Configuration

All configuration is environment variables. A local `.env` file is loaded if
present — see [.env.example](.env.example).

| Variable | Default | Meaning |
|---|---|---|
| `BASE_URL` | *(request `Host`)* | Public origin the rewritten URLs point at, e.g. `https://hls-proxy.example.com`. Falling back to the request host means local runs need no config. |
| `BIND` | `0.0.0.0` | Address to bind. |
| `PORT` | `8080` | Port to listen on. |
| `DEFAULT_EMULATION` | `chrome_137` | Browser profile used for upstream requests. |
| `DEFAULT_EMULATION_OS` | `windows` | Platform that profile presents as. |
| `RUST_LOG` | `hls_proxy=info` | Log filter. |

Set `BASE_URL` when you deploy behind a domain:

```bash
BASE_URL=https://hls-proxy.example.com PORT=8080 hls-proxy
```

Deployment guides for systemd, Docker, nginx and Caddy:
**[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)**.

## How it works

Every proxied URL has the form `/proxy/{token}`, where the token is
base64url-encoded JSON describing what to fetch:

```json
{
  "url": "https://example.com/live/master.m3u8",
  "headers": { "Referer": "https://example.com/" }
}
```

When the proxy fetches a playlist, it rewrites every URL inside it into a *new*
token carrying the same headers. That is the whole trick — context propagates
downward automatically, so a segment request arriving an hour later still knows
exactly which headers it needs.

```
player                    hls-proxy                     origin
  │                           │                            │
  ├── /proxy/{master} ───────►│── GET master.m3u8 ────────►│
  │                           │   + Referer, Origin        │
  │◄── rewritten playlist ────┤◄── #EXTM3U ────────────────┤
  │    (URLs now point here)  │                            │
  │                           │                            │
  ├── /proxy/{segment} ──────►│── GET segment.ts ─────────►│
  │                           │   + the same headers       │
  │◄── streamed bytes ────────┤◄── video data ─────────────┤
```

Playlists are small, so they are buffered and rewritten. Everything else is
streamed straight through without buffering.

Architecture and design decisions: **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Security

**There is no authentication.** Anyone who can reach the server can encode a
payload and use your bandwidth as an open proxy. Keep it on a private network,
put an authenticating reverse proxy in front of it, or restrict access at the
firewall before exposing it publicly.

The SSRF guard rejects loopback, private, link-local (including cloud metadata
at `169.254.169.254`), carrier-NAT and reserved addresses. It understands IPv4
and IPv6, including IPv4-mapped forms like `[::ffff:127.0.0.1]` that name an
IPv4 address in IPv6 syntax, and it blocks the name `localhost`. Every redirect
hop is checked too, not just the URL in the token, so an origin cannot answer
with a `302` to a private address.

**It is still not a hard boundary.** A hostname that resolves to a private
address is fetched, because resolution happens inside the HTTP client where this
check cannot see it. Restrict egress at the network level if that matters — see
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md#hardening).

Proxied responses are returned with `Content-Security-Policy: sandbox` and
`X-Content-Type-Options: nosniff`. Without them, anyone could point the proxy at
an HTML page and have it served from *your* domain, which would run script on
your origin.

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md#hardening) for ways to lock it down.

## Documentation

| Document | Contents |
|---|---|
| [docs/API.md](docs/API.md) | Endpoints, payload schema, status codes, errors |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | systemd, Docker, nginx/Caddy, TLS, hardening |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | How requests flow, design decisions, limits |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | Building, toolchain setup, tests, releasing |

## Contributing

Pull requests are welcome. CI enforces `cargo fmt` and `cargo clippy -D
warnings`, so run this before pushing:

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
```

## Releasing

Bump `version` in [Cargo.toml](Cargo.toml), commit it, then push a `v*` tag —
that tag is what triggers the release build:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Binaries for all six platforms are built on native runners and attached to a
GitHub Release automatically. Full checklist, including how to undo a bad tag:
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md#releasing).

## License

[MIT](LICENSE) © Hamaad Raza
