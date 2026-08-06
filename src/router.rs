//! Per-host routing for `fallback` proxy mode.
//!
//! Live HLS is latency-sensitive: a datacenter proxy on the critical path adds a
//! hop to every playlist refresh and segment fetch. So instead of proxying
//! everything, we send requests direct and only divert a host through the proxy
//! once that host actually rate-limits us (HTTP 429).
//!
//! The state machine is a circuit breaker, inverted. A normal breaker trips to
//! stop calling a failing dependency; this one trips to *change how* we call it —
//! from direct to proxied — for a cooldown, then probes whether direct works
//! again.
//!
//! ```text
//!                    429 on direct
//!      ┌──────────┐ ───────────────► ┌──────────────┐
//!      │  DIRECT  │                  │   PROXIED     │
//!      │ (default)│ ◄─────────────── │ (cooldown t)  │
//!      └──────────┘  direct probe OK └──────┬────────┘
//!           ▲                               │ cooldown expires
//!           │          ┌───────────┐        │
//!           └──────────│ HALF-OPEN │◄───────┘
//!             probe OK │ one probe │
//!                      │ direct,   │──┐ probe 429
//!                      │ rest via  │  │ → longer cooldown
//!                      │ proxy     │◄─┘
//!                      └───────────┘
//! ```
//!
//! State is keyed on the upstream host, so one provider throttling us never
//! slows requests to any other. Time is passed in rather than read from the
//! clock so the whole thing is deterministically testable.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// First cooldown after a host trips. Doubles on each consecutive trip.
const INITIAL_COOLDOWN_SECS: u64 = 30;

/// Ceiling on the exponential backoff, and on any `Retry-After` we honor, so a
/// stuck or hostile header can't pin a host to the proxy for hours.
const MAX_COOLDOWN_SECS: u64 = 900; // 15 minutes

/// How long a claimed half-open probe blocks other requests from probing. If the
/// probing request never reports back (handler error, hung connection), the slot
/// frees itself after this and another request may probe.
const PROBE_TTL: Duration = Duration::from_secs(20);

/// Above this many tracked hosts we sweep out idle, expired entries. Healthy
/// hosts self-clean on recovery, so this only guards against churn from a large
/// number of distinct hosts that each tripped once.
const MAX_ENTRIES: usize = 4096;

/// What the router decided for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Go direct. `probe` marks the single half-open request testing whether a
    /// tripped host has recovered — its outcome resets or re-arms the breaker.
    Direct { probe: bool },
    /// Route through the proxy.
    Proxied,
}

struct HostState {
    /// Consecutive trips, driving the exponential backoff.
    strikes: u32,
    /// Requests to this host are proxied until this instant.
    until: Instant,
    /// Set while a half-open direct probe is outstanding.
    probe_deadline: Option<Instant>,
}

/// Tracks which hosts are currently rate-limiting the direct path.
pub struct HostRouter {
    hosts: Mutex<HashMap<String, HostState>>,
}

impl HostRouter {
    pub fn new() -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Decide how to route the next request to `host`.
    pub fn route(&self, host: &str, now: Instant) -> Decision {
        let mut hosts = self.hosts.lock().unwrap();
        match hosts.get_mut(host) {
            // Never tripped: the fast path, and the overwhelmingly common one.
            None => Decision::Direct { probe: false },
            Some(state) => {
                if now < state.until {
                    // Still cooling down; keep this host on the proxy.
                    Decision::Proxied
                } else {
                    // Cooldown elapsed — half-open. Let exactly one request probe
                    // direct while the rest stay proxied, so a recovered host
                    // doesn't trigger a stampede of concurrent probes all eating
                    // a fresh 429.
                    match state.probe_deadline {
                        Some(deadline) if now < deadline => Decision::Proxied,
                        _ => {
                            state.probe_deadline = Some(now + PROBE_TTL);
                            Decision::Direct { probe: true }
                        }
                    }
                }
            }
        }
    }

    /// Feed back what happened, so the breaker can trip, escalate, or reset.
    ///
    /// `retry_after` is the upstream's `Retry-After` as a duration (zero when
    /// absent); a proxied outcome carries no signal about the direct path and is
    /// ignored here.
    pub fn on_result(
        &self,
        host: &str,
        decision: Decision,
        is_rate_limited: bool,
        retry_after: Duration,
        now: Instant,
    ) {
        let mut hosts = self.hosts.lock().unwrap();
        match decision {
            Decision::Direct { probe } => {
                if is_rate_limited {
                    trip(&mut hosts, host, retry_after, now);
                } else if probe {
                    // A probe came back clean: the host recovered, so drop it
                    // back to the direct fast path for everyone.
                    hosts.remove(host);
                }
                // A non-probe direct success is the healthy steady state — either
                // there is no entry, or a concurrent request just tripped one we
                // must not clobber. Either way, nothing to do.
            }
            // The proxy path tells us nothing about whether direct has recovered.
            Decision::Proxied => {}
        }
        prune(&mut hosts, now);
    }
}

impl Default for HostRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Trip or escalate the breaker for `host`.
fn trip(hosts: &mut HashMap<String, HostState>, host: &str, retry_after: Duration, now: Instant) {
    match hosts.get_mut(host) {
        Some(state) => {
            if now >= state.until {
                // A genuine new trip: either the first strike escalating from a
                // half-open probe, or a re-trip after the cooldown lapsed.
                state.strikes = state.strikes.saturating_add(1);
                let cooldown = backoff(state.strikes).max(retry_after);
                state.until = now + cooldown;
            } else {
                // Already tripped by a concurrent request this round; don't
                // inflate the strike count, but do extend for a longer
                // Retry-After if the upstream asked for one.
                let requested = now + retry_after;
                if requested > state.until {
                    state.until = requested;
                }
            }
            state.probe_deadline = None;
        }
        None => {
            let cooldown = backoff(1).max(retry_after);
            hosts.insert(
                host.to_string(),
                HostState {
                    strikes: 1,
                    until: now + cooldown,
                    probe_deadline: None,
                },
            );
        }
    }
}

/// Exponential backoff: 30s, 60s, 120s, ... capped at 15 minutes.
fn backoff(strikes: u32) -> Duration {
    let shift = strikes.saturating_sub(1).min(20);
    let secs = INITIAL_COOLDOWN_SECS
        .saturating_mul(1u64 << shift)
        .min(MAX_COOLDOWN_SECS);
    Duration::from_secs(secs)
}

/// Interpret a `Retry-After` value, capped so it can never exceed the backoff
/// ceiling. Only the integer-seconds form is understood; the HTTP-date form
/// yields zero and we fall back to plain backoff.
pub fn retry_after_from_secs(value: &str) -> Duration {
    match value.trim().parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs.min(MAX_COOLDOWN_SECS)),
        Err(_) => Duration::ZERO,
    }
}

/// Bounded-size housekeeping: once the map is large, drop entries that are no
/// longer doing anything (cooldown elapsed and no probe outstanding).
fn prune(hosts: &mut HashMap<String, HostState>, now: Instant) {
    if hosts.len() <= MAX_ENTRIES {
        return;
    }
    hosts.retain(|_, state| {
        now < state.until || state.probe_deadline.is_some_and(|deadline| now < deadline)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Duration = Duration::ZERO;

    fn direct() -> Decision {
        Decision::Direct { probe: false }
    }

    #[test]
    fn untracked_host_goes_direct() {
        let router = HostRouter::new();
        let now = Instant::now();
        assert_eq!(
            router.route("cdn.test", now),
            Decision::Direct { probe: false }
        );
    }

    #[test]
    fn a_429_trips_the_host_to_proxied() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        router.on_result("cdn.test", direct(), true, NONE, t0);

        // Still within the initial 30s cooldown.
        assert_eq!(
            router.route("cdn.test", t0 + Duration::from_secs(10)),
            Decision::Proxied
        );
    }

    #[test]
    fn one_host_tripping_does_not_affect_another() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        router.on_result("a.test", direct(), true, NONE, t0);

        assert_eq!(router.route("a.test", t0), Decision::Proxied);
        assert_eq!(
            router.route("b.test", t0),
            Decision::Direct { probe: false }
        );
    }

    #[test]
    fn cooldown_expiry_yields_a_single_probe() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        router.on_result("cdn.test", direct(), true, NONE, t0);

        let after = t0 + Duration::from_secs(31);
        // First request past the cooldown probes direct...
        assert_eq!(
            router.route("cdn.test", after),
            Decision::Direct { probe: true }
        );
        // ...concurrent ones keep using the proxy until the probe reports back.
        assert_eq!(router.route("cdn.test", after), Decision::Proxied);
        assert_eq!(router.route("cdn.test", after), Decision::Proxied);
    }

    #[test]
    fn a_clean_probe_restores_direct() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        router.on_result("cdn.test", direct(), true, NONE, t0);

        let after = t0 + Duration::from_secs(31);
        let probe = router.route("cdn.test", after);
        assert_eq!(probe, Decision::Direct { probe: true });

        router.on_result("cdn.test", probe, false, NONE, after);
        // Recovered: back to the fast path.
        assert_eq!(
            router.route("cdn.test", after),
            Decision::Direct { probe: false }
        );
    }

    #[test]
    fn a_failed_probe_escalates_the_cooldown() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        router.on_result("cdn.test", direct(), true, NONE, t0);

        let after = t0 + Duration::from_secs(31);
        let probe = router.route("cdn.test", after);
        router.on_result("cdn.test", probe, true, NONE, after);

        // Second strike doubles the cooldown to ~60s.
        assert_eq!(
            router.route("cdn.test", after + Duration::from_secs(59)),
            Decision::Proxied
        );
        assert_eq!(
            router.route("cdn.test", after + Duration::from_secs(61)),
            Decision::Direct { probe: true }
        );
    }

    #[test]
    fn retry_after_overrides_a_shorter_backoff() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        // First strike would be 30s, but the upstream asked for 300s.
        router.on_result("cdn.test", direct(), true, Duration::from_secs(300), t0);

        assert_eq!(
            router.route("cdn.test", t0 + Duration::from_secs(120)),
            Decision::Proxied
        );
        assert_eq!(
            router.route("cdn.test", t0 + Duration::from_secs(301)),
            Decision::Direct { probe: true }
        );
    }

    #[test]
    fn concurrent_first_trips_do_not_inflate_strikes() {
        let router = HostRouter::new();
        let t0 = Instant::now();
        // Five segment requests all 429 at once on the first trip.
        for _ in 0..5 {
            router.on_result("cdn.test", direct(), true, NONE, t0);
        }
        // Cooldown must still be the first-strike 30s, not 30s * 2^4.
        assert_eq!(
            router.route("cdn.test", t0 + Duration::from_secs(29)),
            Decision::Proxied
        );
        assert_eq!(
            router.route("cdn.test", t0 + Duration::from_secs(31)),
            Decision::Direct { probe: true }
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(3), Duration::from_secs(120));
        assert_eq!(backoff(100), Duration::from_secs(MAX_COOLDOWN_SECS));
    }

    #[test]
    fn retry_after_parsing_is_capped_and_lenient() {
        assert_eq!(retry_after_from_secs("120"), Duration::from_secs(120));
        assert_eq!(retry_after_from_secs("  45 "), Duration::from_secs(45));
        assert_eq!(
            retry_after_from_secs("999999"),
            Duration::from_secs(MAX_COOLDOWN_SECS)
        );
        // HTTP-date form is not parsed; caller falls back to plain backoff.
        assert_eq!(
            retry_after_from_secs("Wed, 21 Oct 2015 07:28:00 GMT"),
            Duration::ZERO
        );
    }
}
