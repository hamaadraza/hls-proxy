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
pub fn rewrite_playlist(
    body: &str,
    base: &Url,
    payload: &StreamPayload,
    proxy_base: &str,
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
                let replaced = uri_attr_re().replace_all(line, |caps: &Captures| {
                    format!(
                        "URI=\"{}\"",
                        rewrite_uri(&caps[1], base, payload, proxy_base)
                    )
                });
                out.push_str(&replaced);
            } else {
                out.push_str(line);
            }
        } else {
            // A bare line is a variant playlist or a media segment.
            out.push_str(&rewrite_uri(trimmed, base, payload, proxy_base));
        }

        out.push('\n');
    }

    out
}

fn rewrite_uri(raw: &str, base: &Url, payload: &StreamPayload, proxy_base: &str) -> String {
    // Inline keys use data: URIs and must stay as they are.
    if raw.is_empty() || raw.starts_with("data:") {
        return raw.to_string();
    }

    match base.join(raw) {
        Ok(absolute) if matches!(absolute.scheme(), "http" | "https") => {
            proxied_url(proxy_base, &payload.with_url(absolute.as_str()))
        }
        _ => raw.to_string(),
    }
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

    const PROXY: &str = "https://hls-proxy.xyz";

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

    /// Pulls the upstream URL back out of a rewritten proxy line.
    fn decoded_target(line: &str) -> StreamPayload {
        let token = line.rsplit('/').next().unwrap();
        StreamPayload::decode(token).unwrap()
    }

    #[test]
    fn rewrites_relative_variant_playlists() {
        let input = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\n720p/index.m3u8\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY);
        let line = out.lines().nth(2).unwrap();

        assert!(line.starts_with("https://hls-proxy.xyz/proxy/"));
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
        let out = rewrite_playlist(input, &base(), &payload(), PROXY);
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
        let out = rewrite_playlist(input, &base(), &payload(), PROXY);

        let key_line = out.lines().nth(1).unwrap();
        assert!(
            key_line.starts_with("#EXT-X-KEY:METHOD=AES-128,URI=\"https://hls-proxy.xyz/proxy/")
        );
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
        let out = rewrite_playlist(input, &base(), &payload(), PROXY);
        assert_eq!(out.matches("https://hls-proxy.xyz/proxy/").count(), 1);
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
        let out = rewrite_playlist(input, &base(), &payload(), PROXY);
        assert!(out.contains("#EXT-X-VERSION:3"));
        assert!(out.contains("URI=\"data:text/plain;base64,AAAA\""));
        assert!(!out.contains("/proxy/"));
    }

    #[test]
    fn preserves_comments_and_blank_lines() {
        let input = "#EXTM3U\n\n#EXT-X-TARGETDURATION:6\n\nseg.ts\n";
        let out = rewrite_playlist(input, &base(), &payload(), PROXY);
        assert_eq!(out.lines().count(), 5);
        assert_eq!(out.lines().nth(1).unwrap(), "");
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
