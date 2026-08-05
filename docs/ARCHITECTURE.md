# Architecture

How hls-proxy is put together, and why it is built this way.

## The core idea: stateless tokens

Most proxies of this kind keep a session: you register a stream, get an ID back,
and the server remembers which headers belong to that ID. That needs storage,
expiry, and sticky routing when you run more than one instance.

hls-proxy does the opposite. Everything needed to fetch a resource — the URL,
the headers, the browser profile — is encoded into the URL itself:

```
/proxy/{base64url({"url": "...", "headers": {...}})}
```

When a playlist is fetched, every URL inside it is rewritten into a *new* token
carrying the same headers. Context propagates downward on its own:

```
master.m3u8   token{url: master,  headers: H}
  └─ variant  token{url: variant, headers: H}   ← rewritten from the master
       ├─ key token{url: key,     headers: H}   ← rewritten from the variant
       └─ seg  token{url: seg,    headers: H}
```

The consequences are worth stating plainly:

- **No storage.** Nothing to persist, nothing to evict, nothing to expire.
- **Horizontally scalable.** Any instance can serve any request; no shared state,
  no sticky sessions.
- **Survives restarts.** A URL minted before a restart still works after it.
- **URLs are long, and self-describing.** A token is readable by anyone who has
  it, so a proxy URL is as sensitive as the headers inside it.

## Request flow

```
                       ┌─────────────────────────────────────┐
   GET /proxy/{token}  │  1. decode token                    │
  ──────────────────►  │  2. validate URL (scheme, SSRF)     │
                       │  3. pick pooled client (browser+os) │
                       └──────────────┬──────────────────────┘
                                      │  token headers + Range
                                      ▼
                       ┌─────────────────────────────────────┐
                       │           upstream origin           │
                       └──────────────┬──────────────────────┘
                                      │
                       ┌──────────────▼──────────────────────┐
                       │  4. classify the response           │
                       └───────┬───────────────────┬─────────┘
                        playlist                segment
                               │                   │
              ┌────────────────▼──────┐   ┌────────▼─────────────────┐
              │ buffer, rewrite URLs  │   │ stream through unbuffered│
              │ no-cache, m3u8 type   │   │ Range/Content-Range kept │
              └───────────────────────┘   └──────────────────────────┘
```

## Modules

| File | Responsibility |
|---|---|
| [`src/main.rs`](../src/main.rs) | Config from env, router, CORS, startup. Resolves the public base URL. |
| [`src/payload.rs`](../src/payload.rs) | The token: JSON schema, base64 encode/decode, URL validation and SSRF guard. |
| [`src/proxy.rs`](../src/proxy.rs) | Request handlers. Fetches upstream, classifies, streams or rewrites. |
| [`src/rewrite.rs`](../src/rewrite.rs) | Playlist rewriting and content classification. Pure functions, heavily unit-tested. |
| [`src/client.rs`](../src/client.rs) | Pool of HTTP clients, one per browser + platform profile. |

## Design decisions

### Playlists are buffered, everything else is streamed

Rewriting requires the whole playlist in memory, but playlists are kilobytes.
Segments are megabytes and must never be buffered — they are streamed straight
from the upstream response to the client, so memory use stays flat regardless of
how many viewers are connected or how large the segments are.

### Line-based rewriting, not a strict M3U8 parser

Real-world playlists contain vendor extensions, unusual ordering, and outright
malformed lines. A strict parser rejects them; this one rewrites what it
recognises and passes everything else through untouched. A tag we have never
seen cannot break playback.

Two rules cover the whole format:

- A non-comment line is a URI.
- On a `#EXT-` line, any `URI="..."` attribute is a URI.

That second rule is why `EXT-X-KEY`, `EXT-X-MAP`, `EXT-X-MEDIA`, `EXT-X-PART`
and friends all work without being named individually — including tags added to
the spec after this was written.

### Classification prefers the file extension over the content type

Many CDNs serve `.m3u8` files as `application/octet-stream`. Trusting the
content type first would mean streaming a playlist through unrewritten, and
playback would fail with no obvious cause. So a `.m3u8` path wins over a generic
binary content type.

When neither is conclusive, the body is sniffed for `#EXTM3U` — but only for
bodies under 4 MB, so a large segment is never buffered just to identify it.

### Relative URLs resolve against the post-redirect URL

Redirects are followed before rewriting, and relative URLs resolve against
wherever the request actually landed. Origins that redirect a playlist to a
regional CDN would otherwise produce segment URLs pointing at the wrong host.

### `Content-Length` is only forwarded when it is still true

The HTTP client transparently decompresses gzip/brotli/zstd responses. When it
does, the upstream `Content-Length` describes the *compressed* body and would be
wrong for what we send. It is therefore forwarded only when no `Content-Encoding`
was present; otherwise the length is recomputed. A mismatch here truncates video.

### One HTTP client per browser profile

The browser fingerprint is a property of the client, not of the request, so
supporting more than one profile means holding more than one client. They are
built on first use and cached by `browser/os`, and each pools its own
connections. In the common case there is exactly one.

### Browser-identical upstream requests

Upstream requests go out through [wreq](https://github.com/0x676e67/wreq), which
reproduces a real browser's TLS handshake (JA3/JA4), HTTP/2 settings, and header
order. Origins that gate on client fingerprinting see a browser rather than an
HTTP library.

This matters only for origins that actually fingerprint; for everything else it
is invisible. It is also why the project builds BoringSSL from source, which is
the main reason the build has a C toolchain requirement.

## Limits and known gaps

- **No authentication.** The server is an open proxy to anyone who can reach it.
  See [DEPLOYMENT.md](DEPLOYMENT.md#hardening).
- **The SSRF guard is best-effort.** It rejects loopback and private *IP
  literals*, but a hostname resolving to a private address is not caught, since
  resolution happens inside the HTTP client. Do not rely on it as a boundary.
- **No caching.** Every request hits the origin. A CDN or caching reverse proxy
  in front handles this well, since segment URLs are stable and immutable.
- **No DASH.** HLS only.
- **Playlist responses are not compressed**, though upstream responses are
  transparently decompressed.
- **Tokens are not encrypted or signed**, so they can be read and reused by
  anyone who obtains one.
