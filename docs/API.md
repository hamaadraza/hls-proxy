# API reference

Four endpoints. `/proxy/{token}` does the work; the rest exist to help you build
and inspect tokens.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/` | Service info and usage summary |
| `GET` | `/encode` | Build a proxy URL from query parameters |
| `POST` | `/encode` | Build a proxy URL from a JSON body |
| `GET`, `HEAD` | `/proxy/{token}` | Fetch a playlist or segment |

Every response carries permissive CORS headers (`Access-Control-Allow-Origin: *`),
and `OPTIONS` preflight requests are answered, so browser players can use the
proxy directly.

---

## The token

A token is **base64url-encoded JSON** describing one upstream resource:

```json
{
  "url": "https://example.com/live/master.m3u8",
  "headers": {
    "Referer": "https://example.com/",
    "Origin": "https://example.com"
  },
  "emulation": "chrome_137",
  "os": "windows"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `url` | string | **yes** | Upstream `http`/`https` URL to fetch. |
| `headers` | object | no | Extra request headers, sent on every upstream fetch. |
| `emulation` | string | no | Browser profile for the upstream request. Defaults to `DEFAULT_EMULATION`. |
| `os` | string | no | Platform the profile presents as: `windows`, `macos`, `linux`, `android`, `ios`. Defaults to `DEFAULT_EMULATION_OS`. |

Encoding is base64url without padding, but the decoder also accepts standard
base64 and padded input, so tokens produced by other tools generally work.

Tokens are **self-contained**: the proxy stores nothing. This is what lets a
segment request arriving hours later still carry the right headers, and what
lets you run multiple instances behind a load balancer.

Building one by hand is straightforward:

```bash
echo -n '{"url":"https://example.com/master.m3u8","headers":{"Referer":"https://example.com/"}}' \
  | base64 -w0 | tr '+/' '-_' | tr -d '='
```

Or in JavaScript:

```js
const token = btoa(JSON.stringify({
  url: "https://example.com/master.m3u8",
  headers: { Referer: "https://example.com/" },
})).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
```

Tokens are **not encrypted**. Anyone holding one can read the upstream URL and
headers, so treat a proxy URL as being as sensitive as the credentials inside it.

---

## `GET /encode`

Builds a token and the full proxy URL, so you never have to encode by hand.

| Parameter | Repeatable | Description |
|---|---|---|
| `url` | no | **Required.** The upstream stream URL. |
| `header` (or `h`) | yes | A header as `Name:Value`. Only the first `:` splits, so values may contain colons. |
| `emulation` | no | Browser profile, e.g. `chrome_137`. |
| `os` | no | `windows`, `macos`, `linux`, `android`, `ios`. |

```bash
curl "http://localhost:8080/encode\
?url=https://example.com/master.m3u8\
&header=Referer:https://example.com/\
&header=User-Agent:Mozilla/5.0"
```

```json
{
  "url": "http://localhost:8080/proxy/eyJ1cmwiOiJodHRwczovL2V4YW1wbGUu...",
  "payload": "eyJ1cmwiOiJodHRwczovL2V4YW1wbGUu..."
}
```

`url` is what you give the player. `payload` is the bare token, handy if you are
composing URLs yourself.

Remember to URL-encode the parameters when the stream URL contains `&`.

## `POST /encode`

Identical result, but takes JSON — easier for long header sets and for values
that are awkward in a query string.

```bash
curl -X POST http://localhost:8080/encode \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://example.com/master.m3u8",
    "headers": {
      "Referer": "https://example.com/",
      "Cookie": "session=abc123"
    },
    "emulation": "chrome_137",
    "os": "windows"
  }'
```

The base of the returned URL comes from `BASE_URL` if set, and otherwise from
the request's `Host` header (honouring `X-Forwarded-Proto`).

---

## `GET /proxy/{token}`

Fetches the resource named by the token and returns it, rewritten if it is a
playlist. `HEAD` works too.

**What the proxy sends upstream:** the headers from the token, plus your
`Range`, `If-Range`, `If-None-Match` and `If-Modified-Since` headers if you sent
them. Hop-by-hop headers (`Connection`, `Host`, `Transfer-Encoding`, …) are
stripped. Redirects are followed, up to 10, and every hop is re-checked against
the same host rules as the original URL — a redirect to a private address fails
with `502`.

**How the response is classified:**

1. An HLS content type (`application/vnd.apple.mpegurl`, `application/x-mpegurl`,
   `audio/mpegurl`, …) → **playlist**.
2. A `.m3u8` or `.m3u` path → **playlist**. Checked before content type sniffing
   because many CDNs serve playlists as `application/octet-stream`.
3. A media content type (`video/*`, `audio/*`, `application/octet-stream`, …) or
   a media extension (`.ts`, `.m4s`, `.mp4`, `.aac`, `.key`, …) → **segment**.
4. Otherwise the first bytes are checked for `#EXTM3U`. Bodies larger than 4 MB
   skip this check and are treated as segments, so a large file is never
   buffered just to identify it.

**Playlists** are buffered, rewritten, and returned as
`application/vnd.apple.mpegurl` with `Cache-Control: no-cache, no-store,
must-revalidate` — live playlists change constantly and must not be cached. The
upstream `ETag` and `Last-Modified` are *not* passed through, because the rewrite
changes the body and those validators describe the origin's bytes rather than
the ones you receive. A playlist larger than 8 MB is refused with `502`.

**Segments** are streamed through without buffering. `Content-Type`,
`Content-Range`, `Accept-Ranges`, `ETag`, `Last-Modified` and `Cache-Control`
are passed through from the origin, and since your conditional headers are
forwarded upstream, revalidation returns a `304` as normal. `Content-Length` is
passed through only when the body was not decompressed in transit, so it always
matches the bytes sent.

**Every proxied response** also carries `Content-Security-Policy: sandbox` and
`X-Content-Type-Options: nosniff`, so a response cannot execute as script on the
proxy's own origin. This does not affect media playback.

The upstream status code is preserved: a 404 upstream produces a 404 here.

### Range requests

`Range` is forwarded, so seeking in VOD works:

```bash
curl -H "Range: bytes=0-1023" "http://localhost:8080/proxy/{token}"
```

```
HTTP/1.1 206 Partial Content
content-range: bytes 0-1023/1915156
content-length: 1024
```

### What gets rewritten

Inside a playlist, the proxy rewrites:

- **Bare URI lines** — variant playlists in a master, segments in a media playlist.
- **`URI="..."` attributes** on any `#EXT-` tag. This covers `EXT-X-KEY`,
  `EXT-X-MAP`, `EXT-X-MEDIA`, `EXT-X-I-FRAME-STREAM-INF`, `EXT-X-SESSION-KEY`,
  `EXT-X-PART`, `EXT-X-PRELOAD-HINT`, and any future tag using the same form.

Relative URLs resolve against the playlist's **final** URL, after redirects.
Inline `data:` URIs and all other tags and comments pass through untouched.

A rewritten master playlist looks like this:

```
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=2149280,RESOLUTION=1280x720
https://hls-proxy.example.com/proxy/eyJ1cmwiOiJodHRwczovL2V4YW1wbGUuY29tL3YvNzIwcC5tM3U4...
#EXT-X-STREAM-INF:BANDWIDTH=246440,RESOLUTION=320x184
https://hls-proxy.example.com/proxy/eyJ1cmwiOiJodHRwczovL2V4YW1wbGUuY29tL3YvMjQwcC5tM3U4...
```

Each of those tokens carries the original headers, so the player needs to know
nothing about them.

---

## `GET /`

Returns service info and a usage summary. Useful as a health check — it responds
without touching any upstream.

```json
{
  "service": "hls-proxy",
  "usage": { "proxy": "/proxy/{base64url(json)}", "encode": "..." },
  "payload": { "url": "required, the upstream http(s) url", "...": "..." }
}
```

---

## Errors

Errors are JSON with an `error` field:

```json
{ "error": "payload is not valid base64" }
```

| Status | When | Example message |
|---|---|---|
| `400` | Token is not valid base64 | `payload is not valid base64` |
| `400` | Token does not decode to valid JSON | `payload is not valid JSON: ...` |
| `400` | `url` missing or unparseable | `missing required 'url' query parameter` |
| `400` | Scheme is not `http`/`https` | `only http and https urls are supported` |
| `400` | Host is a reserved address or `localhost` | `upstream host is a private, loopback or otherwise reserved address` |
| `400` | Unknown browser profile | `unknown emulation profile 'nope'` |
| `400` | Unknown platform | `unknown emulation os 'solaris' (expected windows, macos, linux, android, ios)` |
| `400` | Malformed `header` parameter | `header 'Referer' must be in 'Name: Value' form` |
| `502` | Upstream unreachable, TLS failure, timeout | `upstream request failed: ...` |
| `502` | Upstream redirected to a blocked host | `upstream request failed: ... redirect blocked: ...` |
| `502` | Upstream body could not be read | `failed to read upstream body: ...` |
| `502` | Playlist too large to rewrite | `playlist is larger than the 8388608 byte rewrite limit` |
| *upstream* | Origin returned an error | passed through unchanged (404, 403, …) |

A `403` from the origin usually means the headers in your token are wrong or
incomplete — check what the origin actually expects, starting with `Referer`.

---

## Browser profiles

Upstream requests are made with a real browser's TLS and HTTP/2 fingerprint, so
origins that reject non-browser clients still serve them. `emulation` accepts any
[wreq-util](https://github.com/0x676e67/wreq-util) profile name in snake_case:

```
chrome_137   chrome_136   chrome_135   ...
firefox_136  firefox_135  ...
safari_18    edge_134     okhttp_5     ...
```

`os` picks the platform the profile presents as, which changes the `User-Agent`
and the client-hint headers to match. Each distinct browser + platform pair gets
its own pooled HTTP client, created on first use.

You rarely need to set either. The defaults (`chrome_137` on `windows`) are the
most common real-world combination.
