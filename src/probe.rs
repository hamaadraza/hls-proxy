//! Per-host segment probing for `auto` segment mode.
//!
//! When a media playlist's segments sit on an open CDN — no special headers, no
//! per-IP tokens — proxying them buys nothing and costs us the whole video's
//! bandwidth twice (CDN → us → viewer). This module decides, per host, whether a
//! stream's segments can be handed to the player as direct upstream URLs instead.
//!
//! The check is a small probe against the first segment, sent from the *direct*
//! client (never the outbound proxy) so it mirrors what the viewer's own player
//! will do, and without the payload's special headers so it tests the segment as
//! an anonymous fetch. The verdict is cached per host with a TTL, single-flighted
//! so concurrent playlist refreshes fire at most one probe.
//!
//! ## What we cannot see
//!
//! Direct segments bypass this server entirely, so if a provider binds segment
//! URLs to the requesting IP our probe (from our IP) can succeed while the
//! viewer's fetch (from theirs) fails, and we never learn of it. This mode is
//! therefore opt-in and meant for providers known to serve IP-agnostic CDN
//! links. Every ambiguous signal resolves to "proxy", the safe default.
//!
//! Time is passed in rather than read from the clock, so the cache is
//! deterministically testable.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a positive verdict (segments are open) is trusted before re-probing.
/// Segment URLs rotate constantly; what we cache is the stable fact that a host
/// doesn't gate them.
const POSITIVE_TTL: Duration = Duration::from_secs(600); // 10 minutes

/// Negative verdicts expire sooner, so a host that was briefly gated (or a probe
/// that hit a transient error) gets another chance without churning probes.
const NEGATIVE_TTL: Duration = Duration::from_secs(120); // 2 minutes

/// A claimed-but-unfinished probe blocks other probers for at most this long,
/// after which the slot self-heals in case the prober never reported back.
const PROBE_TTL: Duration = Duration::from_secs(15);

/// What a probe observed about a host's first segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The segment was fetchable without the payload's headers: a 2xx/206 whose
    /// body was not an HTML error page.
    pub fetchable: bool,
    /// The raw `Access-Control-Allow-Origin` the probe saw, if any. Only consulted
    /// for browser players (requests that carry an `Origin`).
    pub allow_origin: Option<String>,
}

impl Observation {
    /// A probe that failed outright — the conservative, always-proxy verdict.
    pub fn failed() -> Self {
        Self {
            fetchable: false,
            allow_origin: None,
        }
    }
}

/// How a playlist's segments should be emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentPolicy {
    /// Rewrite segments back through this proxy (today's behavior).
    Proxy,
    /// Emit segments as their direct upstream URLs.
    Direct,
}

/// What the caller should do after asking the prober.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// A verdict was available from cache (or a probe is already in flight, in
    /// which case this is `Proxy`).
    Use(SegmentPolicy),
    /// This caller claimed the single-flight slot: run the probe against the
    /// given segment, then report back with `record`.
    Probe,
}

struct Entry {
    observation: Option<Observation>,
    /// When the cached observation stops being trusted.
    expires: Instant,
    /// Set while a probe is in flight; self-heals after `PROBE_TTL`.
    probe_deadline: Option<Instant>,
}

/// Caches, per host, whether segments can be served direct.
pub struct SegmentProber {
    hosts: Mutex<HashMap<String, Entry>>,
}

impl SegmentProber {
    pub fn new() -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Decide how to route this playlist's segments. `origin` is the triggering
    /// request's `Origin` header (present for browser players), which determines
    /// whether CORS is required.
    ///
    /// Returns `Plan::Probe` to exactly one caller when a fresh probe is needed;
    /// everyone else gets a `Plan::Use` (proxying while a probe is outstanding).
    pub fn decide(&self, host: &str, origin: Option<&str>, now: Instant) -> Plan {
        let mut hosts = self.hosts.lock().unwrap();
        if let Some(entry) = hosts.get(host) {
            if let Some(observation) = &entry.observation {
                if now < entry.expires {
                    return Plan::Use(evaluate(observation, origin));
                }
            }
            // A probe is already in flight: proxy this one rather than pile on.
            if entry.probe_deadline.is_some_and(|deadline| now < deadline) {
                return Plan::Use(SegmentPolicy::Proxy);
            }
        }

        // Claim the single-flight probe slot.
        let entry = hosts.entry(host.to_string()).or_insert(Entry {
            observation: None,
            expires: now,
            probe_deadline: None,
        });
        entry.probe_deadline = Some(now + PROBE_TTL);
        Plan::Probe
    }

    /// Store a probe's observation and return the resulting policy for the
    /// origin that triggered it.
    pub fn record(
        &self,
        host: &str,
        observation: Observation,
        origin: Option<&str>,
        now: Instant,
    ) -> SegmentPolicy {
        let policy = evaluate(&observation, origin);
        let ttl = if observation.fetchable {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        let mut hosts = self.hosts.lock().unwrap();
        hosts.insert(
            host.to_string(),
            Entry {
                observation: Some(observation),
                expires: now + ttl,
                probe_deadline: None,
            },
        );
        policy
    }
}

impl Default for SegmentProber {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn a raw observation into a policy for one request.
///
/// Native players (no `Origin`) only need the segment to be fetchable. Browser
/// players additionally need a CORS grant that covers their origin, or their
/// `fetch()` for the segment is blocked and playback stalls.
fn evaluate(observation: &Observation, origin: Option<&str>) -> SegmentPolicy {
    if !observation.fetchable {
        return SegmentPolicy::Proxy;
    }
    match origin {
        None => SegmentPolicy::Direct,
        Some(origin) if cors_allows(observation.allow_origin.as_deref(), origin) => {
            SegmentPolicy::Direct
        }
        Some(_) => SegmentPolicy::Proxy,
    }
}

/// Whether an `Access-Control-Allow-Origin` value covers the given request origin.
fn cors_allows(allow_origin: Option<&str>, request_origin: &str) -> bool {
    match allow_origin {
        Some(value) => value == "*" || value.eq_ignore_ascii_case(request_origin),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Observation {
        Observation {
            fetchable: true,
            allow_origin: None,
        }
    }

    fn open_cors(value: &str) -> Observation {
        Observation {
            fetchable: true,
            allow_origin: Some(value.to_string()),
        }
    }

    #[test]
    fn native_player_gets_direct_when_fetchable() {
        assert_eq!(evaluate(&open(), None), SegmentPolicy::Direct);
    }

    #[test]
    fn browser_needs_cors() {
        // No CORS header: a browser can't fetch it cross-origin, so proxy.
        assert_eq!(
            evaluate(&open(), Some("https://player.test")),
            SegmentPolicy::Proxy
        );
        // Wildcard covers everyone.
        assert_eq!(
            evaluate(&open_cors("*"), Some("https://player.test")),
            SegmentPolicy::Direct
        );
        // Exact echo of the requesting origin is fine...
        assert_eq!(
            evaluate(
                &open_cors("https://player.test"),
                Some("https://player.test")
            ),
            SegmentPolicy::Direct
        );
        // ...but a mismatch (origin-mirroring CDN, different viewer) is not.
        assert_eq!(
            evaluate(
                &open_cors("https://other.test"),
                Some("https://player.test")
            ),
            SegmentPolicy::Proxy
        );
    }

    #[test]
    fn a_failed_probe_always_proxies() {
        assert_eq!(evaluate(&Observation::failed(), None), SegmentPolicy::Proxy);
        assert_eq!(
            evaluate(&Observation::failed(), Some("https://player.test")),
            SegmentPolicy::Proxy
        );
    }

    #[test]
    fn first_caller_probes_and_others_wait() {
        let prober = SegmentProber::new();
        let t0 = Instant::now();

        // First caller claims the probe.
        assert_eq!(prober.decide("cdn.test", None, t0), Plan::Probe);
        // Concurrent callers proxy while it's outstanding.
        assert_eq!(
            prober.decide("cdn.test", None, t0),
            Plan::Use(SegmentPolicy::Proxy)
        );
    }

    #[test]
    fn a_recorded_verdict_is_reused() {
        let prober = SegmentProber::new();
        let t0 = Instant::now();
        prober.decide("cdn.test", None, t0);

        let policy = prober.record("cdn.test", open(), None, t0);
        assert_eq!(policy, SegmentPolicy::Direct);

        // Later callers get the cached verdict without re-probing.
        assert_eq!(
            prober.decide("cdn.test", None, t0 + Duration::from_secs(60)),
            Plan::Use(SegmentPolicy::Direct)
        );
    }

    #[test]
    fn the_verdict_is_evaluated_per_origin() {
        let prober = SegmentProber::new();
        let t0 = Instant::now();
        prober.decide("cdn.test", None, t0);
        // Probed by a native request; CDN advertised no CORS.
        prober.record("cdn.test", open(), None, t0);

        // A native player still gets direct...
        assert_eq!(
            prober.decide("cdn.test", None, t0),
            Plan::Use(SegmentPolicy::Direct)
        );
        // ...but a browser player is proxied, since CORS is unknown.
        assert_eq!(
            prober.decide("cdn.test", Some("https://player.test"), t0),
            Plan::Use(SegmentPolicy::Proxy)
        );
    }

    #[test]
    fn a_positive_verdict_expires_and_reprobes() {
        let prober = SegmentProber::new();
        let t0 = Instant::now();
        prober.decide("cdn.test", None, t0);
        prober.record("cdn.test", open(), None, t0);

        // Still trusted inside the TTL.
        assert_eq!(
            prober.decide("cdn.test", None, t0 + POSITIVE_TTL - Duration::from_secs(1)),
            Plan::Use(SegmentPolicy::Direct)
        );
        // Past it, the next caller re-probes.
        assert_eq!(
            prober.decide("cdn.test", None, t0 + POSITIVE_TTL + Duration::from_secs(1)),
            Plan::Probe
        );
    }

    #[test]
    fn a_negative_verdict_expires_sooner() {
        let prober = SegmentProber::new();
        let t0 = Instant::now();
        prober.decide("cdn.test", None, t0);
        prober.record("cdn.test", Observation::failed(), None, t0);

        // Proxying while the short negative TTL holds.
        assert_eq!(
            prober.decide("cdn.test", None, t0 + Duration::from_secs(30)),
            Plan::Use(SegmentPolicy::Proxy)
        );
        // Then it re-probes rather than proxying forever.
        assert_eq!(
            prober.decide("cdn.test", None, t0 + NEGATIVE_TTL + Duration::from_secs(1)),
            Plan::Probe
        );
    }

    #[test]
    fn a_stalled_probe_slot_self_heals() {
        let prober = SegmentProber::new();
        let t0 = Instant::now();
        // Claim the slot but never record (as if the prober died).
        assert_eq!(prober.decide("cdn.test", None, t0), Plan::Probe);
        assert_eq!(
            prober.decide("cdn.test", None, t0),
            Plan::Use(SegmentPolicy::Proxy)
        );
        // After the probe TTL, another caller may probe again.
        assert_eq!(
            prober.decide("cdn.test", None, t0 + PROBE_TTL + Duration::from_secs(1)),
            Plan::Probe
        );
    }

    #[test]
    fn cors_matching_is_case_insensitive_on_scheme_and_host() {
        assert!(cors_allows(Some("*"), "https://x.test"));
        assert!(cors_allows(Some("https://X.Test"), "https://x.test"));
        assert!(!cors_allows(None, "https://x.test"));
        assert!(!cors_allows(Some("https://y.test"), "https://x.test"));
    }
}
