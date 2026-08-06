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
                       │  3. pick pooled client (browser+os, │
                       │     direct or proxied per policy)   │
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
| [`src/client.rs`](../src/client.rs) | Pool of HTTP clients, one per browser + platform profile, each in a direct and (if configured) a proxied variant. Proxy parsing and redaction. |
| [`src/router.rs`](../src/router.rs) | `fallback` mode's per-host rate-limit breaker: decides direct vs proxied and reacts to 429s. Pure state machine, deterministically tested. |
| [`src/probe.rs`](../src/probe.rs) | `auto` segment mode's per-host cache: decides whether a host's segments can be served direct, from a probe observation. Pure logic, deterministically tested. |

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

### The buffering limit is enforced while reading, not from `Content-Length`

The 4 MB sniff threshold above is only consulted when the upstream declared a
`Content-Length`, and that is both frequently absent — any chunked response — and
attacker-controlled. On its own it therefore bounds nothing: a chunked body with
no content type would be read into memory in full.

So a second limit, `MAX_BUFFER_BYTES` (8 MB), is applied *as the body is read*.
Past it, the prefix already in memory is rejoined to the rest of the stream and
the response is forwarded without ever being fully resident. A body that claimed
to be a playlist and then exceeded the limit returns `502` naming the limit,
because forwarding a playlist unrewritten breaks playback with no visible cause.

### Every redirect hop is validated, not just the first URL

The SSRF guard runs on the URL in the token, but redirects are followed *inside*
the HTTP client, where that check cannot reach. A custom redirect policy re-runs
the same guard on each hop, so an origin cannot answer with a `302` to
`http://127.0.0.1:6379/` and reach something the first check would have refused.
Because a custom policy replaces the built-in one, it enforces its own hop limit.

### Relative URLs resolve against the post-redirect URL

Redirects are followed before rewriting, and relative URLs resolve against
wherever the request actually landed. Origins that redirect a playlist to a
regional CDN would otherwise produce segment URLs pointing at the wrong host.

### Proxied responses are isolated from this origin

Responses carry whatever content type the upstream sent, and there is no
authentication, so the proxy would otherwise serve attacker-chosen `text/html`
from your own domain — script running on your origin. Every proxied response
gets `Content-Security-Policy: sandbox`, which puts a document response in an
opaque origin, and `X-Content-Type-Options: nosniff`. Neither affects playback:
the `sandbox` directive applies to documents, not to segments fetched by a player.

The headers are set inside `forward_response_headers` rather than at each call
site, so no response path can be added later that quietly omits them.

### Rewritten playlists do not keep the upstream validators

A rewritten playlist is not the entity the origin described, so its `ETag` and
`Last-Modified` are dropped rather than passed through — an `ETag` for the
original bytes is a lie about the ones we send, and a caching layer in front of
the proxy would act on it. Segments are forwarded byte-for-byte, so they keep
theirs, and the client's `If-None-Match`/`If-Modified-Since` are forwarded
upstream so that revalidation can still produce a `304`.

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

### Proxy modes and per-host fallback

An optional outbound proxy (`PROXY_URL`) lets upstream traffic leave from another
IP, which is how a single provider's `429`s are avoided. It is baked into the
client rather than attached per request, so its `Proxy-Authorization` lands
correctly for both http and https upstreams; the direct and proxied clients are
cached separately and picked per request.

`PROXY_MODE=always` routes everything through the proxy. `fallback` keeps the
proxy off the critical path for latency-sensitive live HLS: [`router.rs`](../src/router.rs)
holds a per-host circuit breaker, inverted — a host is sent direct until it
returns a `429`, at which point that one request is retried through the proxy and
the host is routed proxied for a cooldown. The cooldown backs off exponentially
(honoring `Retry-After`), and a single half-open probe checks for recovery so a
cooled-down host doesn't draw a stampede of concurrent probes. The state machine
takes the current time as a parameter, which is what makes it deterministically
testable without sleeping.

### Serving segments direct (`auto` segment mode)

For streams whose segments live on an open CDN, relaying the media through this
server costs the whole video's bandwidth twice and adds a hop to every segment.
`SEGMENT_MODE=auto` ([`probe.rs`](../src/probe.rs)) probes the first segment of
each media playlist — a one-byte `Range` fetch on the *direct* client (the path
the viewer uses, never the outbound proxy) and without the payload's special
headers — and, if it comes back cleanly, rewrites that playlist to point segments
straight at the CDN. Verdicts are cached per host and single-flighted, so it's one
probe per host, not per segment.

Only media bytes are released: segment lines and the media-carrying `URI` tags
(`EXT-X-MAP`, `EXT-X-PART`, `EXT-X-PRELOAD-HINT`). Nested playlists (anything
resolving to `.m3u8`) and `EXT-X-KEY` decryption keys always stay proxied — the
rewritten playlist is the only lever we keep once segments bypass us, so we don't
give it up. Direct URLs are always absolutized, since a relative URI would resolve
against this proxy's origin. Browser players (an `Origin` on the request) also
require a matching CORS grant on the probe, or the segment `fetch()` would be
blocked; native players don't.

The blind spot is deliberate and documented: the probe runs from our IP, the
viewer fetches from theirs, so an IP-locked provider passes the probe yet fails
the viewer with no signal back to us. Hence it is opt-in, every ambiguous signal
resolves to proxying, and it composes cleanly with `PROXY_MODE` — a host that
needs the proxy even to fetch a segment simply fails the direct probe and keeps
being proxied.

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
- **The SSRF guard is best-effort.** It rejects reserved *IP literals* on every
  hop, but a hostname resolving to a private address is not caught, since
  resolution happens inside the HTTP client. Do not rely on it as a boundary.
- **No caching.** Every request hits the origin. A CDN or caching reverse proxy
  in front handles this well, since segment URLs are stable and immutable.
- **No DASH.** HLS only.
- **Playlist responses are not compressed**, though upstream responses are
  transparently decompressed.
- **Tokens are not encrypted or signed**, so they can be read and reused by
  anyone who obtains one.
