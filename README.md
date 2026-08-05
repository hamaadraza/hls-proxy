# hls-proxy

[![CI](https://github.com/hamaadraza/hls-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/hamaadraza/hls-proxy/actions/workflows/ci.yml)
[![Release](https://github.com/hamaadraza/hls-proxy/actions/workflows/release.yml/badge.svg)](https://github.com/hamaadraza/hls-proxy/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

An HLS reverse proxy in Rust. You hand it a stream URL plus whatever headers
that stream demands; it hands back a playlist whose every URL points at your own
domain — and it keeps those headers attached to every variant playlist, segment,
encryption key and init segment that follows.

Upstream requests go out through [`wreq`](https://github.com/0x676e67/wreq) with
Chrome emulation, so the TLS (JA3/JA4) and HTTP/2 fingerprints match a real
browser instead of a Rust HTTP client.

## Why

Two things break browser playback of protected HLS streams:

1. **Headers.** The origin wants a `Referer`/`Origin`/`User-Agent`, on *every*
   request. A browser player won't send custom headers for segments, and CORS
   blocks the request anyway.
2. **Fingerprints.** Even with perfect headers, origins increasingly reject
   clients whose TLS handshake doesn't look like a browser.

This proxy handles both, and does it statelessly: everything needed to fetch a
resource is encoded in its URL, so there is no session store, nothing to expire,
and you can run as many instances behind a load balancer as you like.

## Install

Grab a prebuilt binary from the [releases page](../../releases) — it is a single
self-contained executable with no runtime dependencies:

| Platform | Architecture | Archive |
|---|---|---|
| Linux | x86_64 | `hls-proxy-<tag>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | arm64 | `hls-proxy-<tag>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Intel | `hls-proxy-<tag>-x86_64-apple-darwin.tar.gz` |
| macOS | Apple silicon | `hls-proxy-<tag>-aarch64-apple-darwin.tar.gz` |
| Windows | x86_64 | `hls-proxy-<tag>-x86_64-pc-windows-msvc.zip` |
| Windows | arm64 | `hls-proxy-<tag>-aarch64-pc-windows-msvc.zip` |

Each archive ships with a `.sha256` checksum file. Linux binaries are built on
Ubuntu 22.04 (glibc 2.35) for broad compatibility.

Or build from source:

## Quick start

```bash
cargo run
```

Then build a playable URL:

```bash
curl "http://localhost:8080/encode?url=https://example.com/master.m3u8&header=Referer:https://example.com/"
```

```json
{
  "url": "http://localhost:8080/proxy/eyJ1cmwiOiJodHRwczovL2V4YW1wbGUu...",
  "payload": "eyJ1cmwiOiJodHRwczovL2V4YW1wbGUu..."
}
```

Feed that `url` to hls.js, VLC, ffmpeg, or a `<video>` tag.

## How it works

Every proxied URL is `/proxy/{token}`, where the token is base64url-encoded JSON:

```json
{
  "url": "https://origin.example.com/live/master.m3u8",
  "headers": { "Referer": "https://origin.example.com/" },
  "emulation": "chrome_137",
  "os": "windows"
}
```

Only `url` is required. When the proxy fetches a playlist it rewrites every URL
inside it into a *new* token carrying the same headers, emulation and os — which
is what makes the context propagate all the way down to the last segment.

Playlists are buffered and rewritten. Everything else (segments, keys, init
segments) is streamed straight through without buffering, with `Range` requests
forwarded so seeking works. Responses carry permissive CORS headers.

Rewriting covers bare URI lines and the `URI="..."` attribute of any `#EXT-`
tag, so `EXT-X-KEY`, `EXT-X-MAP`, `EXT-X-MEDIA`, `EXT-X-I-FRAME-STREAM-INF`,
`EXT-X-PART` and friends all work. Inline `data:` URIs are left alone.

## Routes

| Route | Purpose |
|---|---|
| `GET /proxy/{token}` | The proxy. Playlists rewritten, everything else streamed. |
| `GET /encode?url=…&header=Name:Value` | Builds a token and full URL. Repeat `header` as needed; optional `emulation` and `os`. |
| `POST /encode` | Same, with a JSON body — easier for long header sets. |
| `GET /` | Usage / health. |

```bash
curl -X POST http://localhost:8080/encode \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com/master.m3u8","headers":{"Referer":"https://example.com/"}}'
```

## Configuration

Read from the environment (a local `.env` is loaded if present — see
`.env.example`):

| Variable | Default | Meaning |
|---|---|---|
| `BASE_URL` | *(request `Host`)* | Public origin that rewritten URLs point at, e.g. `https://hls-proxy.example.com`. Falling back to the request host means local runs need no config. |
| `BIND` | `0.0.0.0` | Bind address. |
| `PORT` | `8080` | Port. |
| `DEFAULT_EMULATION` | `chrome_137` | Any [wreq-util](https://github.com/0x676e67/wreq-util) profile. |
| `DEFAULT_EMULATION_OS` | `windows` | `windows`, `macos`, `linux`, `android`, `ios`. |
| `RUST_LOG` | `hls_proxy=info` | Log filter. |

Behind a reverse proxy, either set `BASE_URL` or forward `X-Forwarded-Proto` so
generated URLs use `https`.

## Fingerprint check

Point the proxy at `tls.peet.ws` and compare:

```bash
curl "http://localhost:8080/encode?url=https://tls.peet.ws/api/all"
# then fetch the returned url
```

Verified output for the default profile:

| | through the proxy | plain curl |
|---|---|---|
| JA4 | `t13d1516h2_8daaf6152771_d8a2da3f94cd` | `t13d2012_2b729b4bf6f3_e24568c0d440` |
| Akamai HTTP/2 | `52d84b11737d980aef856699f885ca86` | *(no HTTP/2)* |
| User-Agent | `Mozilla/5.0 (Windows NT 10.0; Win64; x64) …Chrome…` | `curl/8.12.1` |

## Notes and limits

- **No authentication.** Anyone who can reach the server can encode a payload
  and use your bandwidth. Put it behind a firewall, or add a shared secret
  before exposing it publicly.
- **SSRF guard is best-effort.** Loopback and private *IP literals* are
  rejected, but a hostname that resolves to a private address is not, since
  resolution happens inside the HTTP client.
- **Emulation is per-client, not per-request**, so each distinct
  `emulation`/`os` pair lazily builds and caches its own pooled client.

## Building from source

`wreq` compiles BoringSSL, so you need **cmake**, a **C/C++ toolchain**, and
**libclang** (for bindgen).

- **Linux:** `sudo apt-get install cmake clang libclang-dev`
- **macOS:** `brew install cmake` (Xcode command line tools supply clang)
- **Windows:** cmake, the MSVC C++ workload, and LLVM. If the build reports
  "Unable to find libclang", set `LIBCLANG_PATH` to a directory containing
  `libclang.dll`.

### The BoringSSL assembly backend

Assembly is used whenever it can be. On Linux and macOS that is always, and
`cmake/boringssl-asm-fallback.cmake` does nothing at all.

Windows is the exception: BoringSSL assembles its x86/x86_64 crypto with **NASM**,
and `boring-sys2` only disables assembly automatically when cross-compiling — so
a native Windows build fails outright without NASM. The shim detects this:

- **NASM on `PATH`** → nothing is changed, and BoringSSL builds with assembly.
- **NASM missing** → falls back to the portable C backend so the build succeeds.

To get the faster build on Windows, install NASM in an elevated shell and
rebuild — no config change needed, the shim picks it up automatically:

```bash
choco install nasm
```

Then reopen your terminal and force a reconfigure (a stale `CMakeCache.txt`
would otherwise keep the previous decision):

```bash
cargo clean -p boring-sys2
```

The fallback costs raw AES/SHA throughput. It does **not** change the TLS
JA3/JA4 fingerprint, which comes from cipher suites, extensions and HTTP/2
settings rather than from how the crypto is compiled.

### Cross-compiling

`.cargo/config.toml` sets `CMAKE_TOOLCHAIN_FILE`, and `boring-sys2` skips its own
cross-compile cmake setup when that variable is set. If you build for a target
other than your host, remove the `[env]` section. The release workflow sidesteps
this by building every target on a native runner.

## CI and releases

`.github/workflows/ci.yml` runs on every push and pull request: `rustfmt`,
`clippy -D warnings`, the test suite on Linux/Windows/macOS, and a smoke test
that boots the binary and calls `/encode`. Windows CI installs NASM, so the
assembly path is exercised there too.

`.github/workflows/release.yml` builds all six targets on native runners and
publishes them. To cut a release:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Artifacts are uploaded to a GitHub Release with generated notes and checksums.
Running the workflow manually builds the archives without publishing, unless it
is run from a tag — handy for rehearsing a release.

The `aarch64-pc-windows-msvc` target is marked experimental and cannot block a
release: BoringSSL's Windows assembly path is x86-only, so that target builds
the portable C backend.

## Tests

```bash
cargo test
```

Covers payload round-tripping, the SSRF guard, playlist rewriting (relative and
absolute URLs, `EXT-X-KEY`/`EXT-X-MAP`, multiple URIs per line, `data:` URIs,
comment preservation), content classification, and base-URL resolution.

## Contributing

Pull requests are welcome. CI enforces `cargo fmt` and `cargo clippy -D
warnings`, so run both before pushing:

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

[MIT](LICENSE) © Hamaad Raza
