use crate::payload::StreamPayload;
use regex::{Captures, Regex};
use std::sync::OnceLock;
use url::Url;

fn uri_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"URI="([^"]*)""#).unwrap())
}

/// Rewrites every URL in an HLS playlist to point back at this proxy, carrying
/// the original headers and emulation profile into each one.
///
/// Line-based rather than a strict M3U8 parse: real-world playlists contain
/// vendor tags and malformed lines that a strict parser rejects, and anything
/// we don't recognise should pass through untouched rather than break playback.
///
/// When `direct_origin` is set, media bytes on **that same origin** — bare
/// segment lines and the media-carrying `URI="..."` tags (`EXT-X-MAP`,
/// `EXT-X-PART`, `EXT-X-PRELOAD-HINT`) — are emitted as their absolute upstream
/// URLs so the player fetches them straight from the CDN. Everything that is our
/// point of control still routes through the proxy: nested playlists (anything
/// resolving to `.m3u8`), rendition/I-frame `URI`s, and decryption keys.
///
/// The origin scoping matters: only the probed origin is known to serve its
/// segments openly, so a playlist interleaving another CDN — or the same host on
/// a different scheme or port, which is a different security surface entirely —
/// keeps those proxied rather than assuming the verdict transfers.
pub fn rewrite_playlist(
    body: &str,
    base: &Url,
    payload: &StreamPayload,
    proxy_base: &str,
    direct_origin: Option<&Url>,
) -> String {
    let mut out = String::with_capacity(body.len() * 2);

    for line in body.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            out.push_str(line);
        } else if trimmed.starts_with('#') {
            // Covers EXT-X-KEY, EXT-X-MAP, EXT-X-MEDIA, EXT-X-I-FRAME-STREAM-INF,
            // EXT-X-SESSION-KEY, EXT-X-PART, EXT-X-PRELOAD-HINT and anything
            // later that follows the same URI="..." convention.
            if trimmed.contains("URI=\"") {
                let upper = trimmed.to_ascii_uppercase();
                // Only media-carrying tags may go direct; keys and playlist
                // pointers stay proxied.
                let uri_origin = direct_origin.filter(|_| tag_carries_media(&upper));
                let relative = tag_requires_relative_uri(&upper);
                let replaced = uri_attr_re().replace_all(line, |caps: &Captures| {
                    let rewritten = if relative {
                        relative_proxy_uri(&caps[1], base, payload)
                    } else {
                        rewrite_uri(&caps[1], base, payload, proxy_base, uri_origin)
                    };
                    format!("URI=\"{rewritten}\"")
                });
                out.push_str(&replaced);
            } else {
                out.push_str(line);
            }
        } else {
            // A bare line is a variant playlist or a media segment. Under the
            // direct policy it's a segment (masters are never probed), but the
            // playlist-extension guard in rewrite_uri keeps any stray nested
            // playlist proxied regardless.
            out.push_str(&rewrite_uri(
                trimmed,
                base,
                payload,
                proxy_base,
                direct_origin,
            ));
        }

        out.push('\n');
    }

    out
}

/// Whether a `URI="..."` tag points at media bytes (eligible for direct serving)
/// rather than a key or a nested playlist. Unknown tags are treated as not media,
/// so the conservative proxied path is the default.
fn tag_carries_media(upper: &str) -> bool {
    upper.starts_with("#EXT-X-MAP")
        || upper.starts_with("#EXT-X-PART:")
        || upper.starts_with("#EXT-X-PRELOAD-HINT")
}

/// `EXT-X-RENDITION-REPORT`'s URI *must* be relative to the playlist containing
/// it (RFC 8216bis §4.4.5.4), so the absolute proxy URL used everywhere else
/// would make the tag invalid and strict LL-HLS clients may reject it.
fn tag_requires_relative_uri(upper: &str) -> bool {
    upper.starts_with("#EXT-X-RENDITION-REPORT")
}

/// A proxied URI expressed relative to the playlist's own proxy URL.
///
/// Playlists are served as `{base}/proxy/{token}`, so a bare token resolves
/// against that by replacing the last path segment — landing on
/// `{base}/proxy/{other token}`, exactly where the absolute form would have
/// pointed, while staying the relative URI the spec demands. This also survives a
/// `BASE_URL` with a path prefix, and the client appending query directives.
fn relative_proxy_uri(raw: &str, base: &Url, payload: &StreamPayload) -> String {
    if raw.is_empty() || raw.starts_with("data:") {
        return raw.to_string();
    }

    match base.join(raw) {
        Ok(absolute) if matches!(absolute.scheme(), "http" | "https") => {
            payload.with_url(absolute.as_str()).encode()
        }
        _ => raw.to_string(),
    }
}

fn rewrite_uri(
    raw: &str,
    base: &Url,
    payload: &StreamPayload,
    proxy_base: &str,
    direct_origin: Option<&Url>,
) -> String {
    // Inline keys use data: URIs and must stay as they are.
    if raw.is_empty() || raw.starts_with("data:") {
        return raw.to_string();
    }

    match base.join(raw) {
        Ok(absolute) if matches!(absolute.scheme(), "http" | "https") => {
            if may_serve_direct(&absolute, direct_origin) {
                absolute.to_string()
            } else {
                proxied_url(proxy_base, &payload.with_url(absolute.as_str()))
            }
        }
        _ => raw.to_string(),
    }
}

/// Whether one resolved URI may be handed to the player directly: it has to sit
/// on the exact origin we probed, and it must not be a nested playlist — those
/// are our only lever once segments bypass us, so they stay proxied.
fn may_serve_direct(absolute: &Url, direct_origin: Option<&Url>) -> bool {
    let Some(direct_origin) = direct_origin else {
        return false;
    };
    absolute.scheme() == direct_origin.scheme()
        && absolute.host_str() == direct_origin.host_str()
        && absolute.port_or_known_default() == direct_origin.port_or_known_default()
        && !has_playlist_extension(absolute)
}

pub fn proxied_url(proxy_base: &str, payload: &StreamPayload) -> String {
    format!(
        "{}/proxy/{}",
        proxy_base.trim_end_matches('/'),
        payload.encode()
    )
}

/// True when the content type names an HLS playlist.
pub fn is_playlist_content_type(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        ct.as_str(),
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
            | "application/mpegurl"
            | "vnd.apple.mpegurl"
    )
}

pub fn has_playlist_extension(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u")
}

/// True when the body itself announces a playlist, used when headers and the
/// file extension are both inconclusive (common with tokenised CDN URLs).
pub fn body_looks_like_playlist(body: &[u8]) -> bool {
    let head = &body[..body.len().min(64)];
    let text = String::from_utf8_lossy(head);
    text.trim_start().starts_with("#EXTM3U")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::StreamPayload;
    use std::collections::BTreeMap;

    const PROXY: &str = "https://hls-proxy.example.com";

    fn payload() -> StreamPayload {
        let mut headers = BTreeMap::new();
        headers.insert("Referer".to_string(), "https://origin.test/".to_string());
        StreamPayload {
            url: "https://origin.test/live/master.m3u8".to_string(),
            headers,
            emulation: Some("chrome_137".to_string()),
            os: Some("windows".to_string()),
        }
    }

    fn base() -> Url {
        Url::parse("https://origin.test/live/master.m3u8").unwrap()
    }

    /// Stands in for the probed segment: the origin cleared for direct serving.
    fn origin_ref() -> Url {
        Url::parse("https://origin.test/live/probed.ts").unwrap()
    }

    /// Pulls the upstream URL back out of a rewritten proxy line.
    fn decoded_target(line: &str) -> StreamPayload {
        let token = line.rsplit('/').next().unwrap();
        StreamPayload::decode(token).unwrap()
    }

    #[test]
    fn rewrites_relative_variant_playlists() {
        let input = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\n720p/index.m3u8\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        let line = out.lines().nth(2).unwrap();

        assert!(line.starts_with("https://hls-proxy.example.com/proxy/"));
        let target = decoded_target(line);
        assert_eq!(target.url, "https://origin.test/live/720p/index.m3u8");
        assert_eq!(
            target.headers.get("Referer").map(String::as_str),
            Some("https://origin.test/")
        );
        assert_eq!(target.emulation.as_deref(), Some("chrome_137"));
    }

    #[test]
    fn rewrites_absolute_segment_urls() {
        let input = "#EXTM3U\n#EXTINF:6.0,\nhttps://cdn.test/s/1.ts\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        let target = decoded_target(out.lines().nth(2).unwrap());
        assert_eq!(target.url, "https://cdn.test/s/1.ts");
    }

    #[test]
    fn rewrites_key_and_map_uris() {
        let input = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"../key.bin\",IV=0x0\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXTINF:4.0,\n",
            "seg1.m4s\n"
        );
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);

        let key_line = out.lines().nth(1).unwrap();
        assert!(key_line
            .starts_with("#EXT-X-KEY:METHOD=AES-128,URI=\"https://hls-proxy.example.com/proxy/"));
        assert!(key_line.ends_with("\",IV=0x0"));

        let key_uri = key_line
            .split("URI=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(decoded_target(key_uri).url, "https://origin.test/key.bin");

        let map_uri = out
            .lines()
            .nth(2)
            .unwrap()
            .split("URI=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(
            decoded_target(map_uri).url,
            "https://origin.test/live/init.mp4"
        );
    }

    #[test]
    fn rewrites_multiple_uris_on_one_line() {
        let input = "#EXT-X-MEDIA:TYPE=AUDIO,URI=\"a/audio.m3u8\",GROUP-ID=\"aud\"\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        assert_eq!(
            out.matches("https://hls-proxy.example.com/proxy/").count(),
            1
        );
        assert!(out.contains("GROUP-ID=\"aud\""));
    }

    #[test]
    fn leaves_data_uris_and_plain_tags_alone() {
        let input = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"data:text/plain;base64,AAAA\"\n",
            "#EXT-X-ENDLIST\n"
        );
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        assert!(out.contains("#EXT-X-VERSION:3"));
        assert!(out.contains("URI=\"data:text/plain;base64,AAAA\""));
        assert!(!out.contains("/proxy/"));
    }

    #[test]
    fn preserves_comments_and_blank_lines() {
        let input = "#EXTM3U\n\n#EXT-X-TARGETDURATION:6\n\nseg.ts\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        assert_eq!(out.lines().count(), 5);
        assert_eq!(out.lines().nth(1).unwrap(), "");
    }

    /// Under the direct policy a media playlist's segments become their absolute
    /// upstream URLs — never relative (which would resolve against our origin).
    #[test]
    fn direct_policy_emits_absolute_segment_urls() {
        let input = "#EXTM3U\n#EXTINF:6.0,\nseg/1.ts\n#EXTINF:6.0,\nhttps://origin.test/2.ts\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, Some(&origin_ref()));
        let lines: Vec<&str> = out.lines().collect();

        // Relative segment resolved against the playlist's own URL.
        assert_eq!(lines[2], "https://origin.test/live/seg/1.ts");
        // Absolute segment on the same host passed straight through.
        assert_eq!(lines[4], "https://origin.test/2.ts");
        assert!(!out.contains("/proxy/"));
    }

    /// The verdict belongs to the host we probed. A playlist that interleaves
    /// segments from another CDN must keep those proxied — that host was never
    /// tested and may well be gated.
    #[test]
    fn direct_policy_is_scoped_to_the_probed_host() {
        let input = concat!(
            "#EXTM3U\n",
            "#EXTINF:6.0,\n",
            "https://origin.test/1.ts\n",
            "#EXTINF:6.0,\n",
            "https://other-cdn.test/2.ts\n"
        );
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, Some(&origin_ref()));
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[2], "https://origin.test/1.ts");
        assert!(
            lines[4].starts_with("https://hls-proxy.example.com/proxy/"),
            "an unprobed host must stay proxied, got {}",
            lines[4]
        );
        assert_eq!(decoded_target(lines[4]).url, "https://other-cdn.test/2.ts");
    }

    /// Same host, different scheme or port, is a different origin with its own
    /// auth and CORS surface — the verdict must not carry over to it.
    #[test]
    fn direct_policy_does_not_cross_scheme_or_port() {
        let input = concat!(
            "#EXTM3U\n",
            "#EXTINF:6.0,\n",
            "https://origin.test/ok.ts\n",
            "#EXTINF:6.0,\n",
            "http://origin.test/plain.ts\n",
            "#EXTINF:6.0,\n",
            "https://origin.test:8443/other-port.ts\n"
        );
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, Some(&origin_ref()));
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[2], "https://origin.test/ok.ts");
        assert!(lines[4].starts_with("https://hls-proxy.example.com/proxy/"));
        assert!(lines[6].starts_with("https://hls-proxy.example.com/proxy/"));
    }

    /// Keys and nested playlists stay proxied even when segments go direct; the
    /// init segment (EXT-X-MAP) follows the segments out.
    #[test]
    fn direct_policy_still_proxies_keys_and_playlists() {
        let input = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXTINF:4.0,\n",
            "seg1.m4s\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio/a.m3u8\",GROUP-ID=\"aud\"\n"
        );
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, Some(&origin_ref()));
        let lines: Vec<&str> = out.lines().collect();

        // Key material is never handed out directly.
        assert!(lines[1].contains("URI=\"https://hls-proxy.example.com/proxy/"));
        // Init segment is media, so it goes direct.
        assert_eq!(
            lines[2],
            "#EXT-X-MAP:URI=\"https://origin.test/live/init.mp4\""
        );
        // Segment goes direct.
        assert_eq!(lines[4], "https://origin.test/live/seg1.m4s");
        // A rendition sub-playlist stays proxied — it's a .m3u8, our control point.
        assert!(lines[5].contains("URI=\"https://hls-proxy.example.com/proxy/"));
    }

    /// The proxy policy is exactly today's behavior: everything is rewritten back
    /// through the proxy.
    #[test]
    fn proxy_policy_rewrites_segments_as_before() {
        let input = "#EXTM3U\n#EXTINF:6.0,\nhttps://cdn.test/1.ts\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        let target = decoded_target(out.lines().nth(2).unwrap());
        assert_eq!(target.url, "https://cdn.test/1.ts");
    }

    /// The spec requires this URI to be relative, and a bare token is: resolved
    /// against the playlist's own `{base}/proxy/{token}` URL it lands on the
    /// proxied rendition, which is where the absolute form pointed.
    #[test]
    fn rendition_report_uri_stays_relative_and_still_resolves() {
        let input = "#EXTM3U\n#EXT-X-RENDITION-REPORT:URI=\"../720p/index.m3u8\",LAST-MSN=42\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY, None);
        let line = out.lines().nth(1).unwrap();

        let uri = line
            .split("URI=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(
            !uri.contains('/') && !uri.starts_with("http"),
            "must be a bare relative token, got {uri}"
        );
        assert!(line.ends_with(",LAST-MSN=42"), "other attributes survive");

        // It points where the absolute form would have.
        assert_eq!(
            decoded_target(uri).url,
            "https://origin.test/720p/index.m3u8"
        );

        // And a player resolving it against the playlist URL we served lands on
        // the proxied rendition.
        let served_at = Url::parse(&format!("{PROXY}/proxy/{}", payload().encode())).unwrap();
        assert_eq!(
            served_at.join(uri).unwrap().as_str(),
            format!("{PROXY}/proxy/{uri}")
        );
    }

    /// The same resolution has to hold when the proxy is mounted under a path
    /// prefix and the client appended blocking-reload directives.
    #[test]
    fn relative_rendition_uri_survives_prefixes_and_query() {
        let token = payload().encode();
        let served_at =
            Url::parse(&format!("https://host.test/hls/proxy/{token}?_HLS_msn=9")).unwrap();
        assert_eq!(
            served_at.join("OTHERTOKEN").unwrap().as_str(),
            "https://host.test/hls/proxy/OTHERTOKEN"
        );
    }

    #[test]
    fn detects_playlists_by_content_type_extension_and_body() {
        assert!(is_playlist_content_type("application/vnd.apple.mpegurl"));
        assert!(is_playlist_content_type(
            "application/x-mpegURL; charset=utf-8"
        ));
        assert!(!is_playlist_content_type("video/mp2t"));

        assert!(has_playlist_extension(
            &Url::parse("https://a.test/x.M3U8?token=1").unwrap()
        ));
        assert!(!has_playlist_extension(
            &Url::parse("https://a.test/x.ts").unwrap()
        ));

        assert!(body_looks_like_playlist(b"#EXTM3U\n#EXT-X-VERSION:3"));
        assert!(!body_looks_like_playlist(b"\x47\x40\x00\x10"));
        assert!(!body_looks_like_playlist(b""));
    }
}
