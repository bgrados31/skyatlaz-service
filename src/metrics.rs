//! The counters the service publishes at `/metrics`, in the Prometheus text
//! format.
//!
//! Two rules shape this module. First, every label value comes from a closed set
//! defined here and never from a string a caller sent: a label whose value a
//! client picks is how a metrics endpoint turns into a memory leak, and this
//! service bounds every structure a request can reach. A path that is not one of
//! the routes below is `other`, and a status is one of three classes. Second, the
//! table is a fixed array of atomics built once, so scraping costs no allocation
//! beyond the string it renders.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The routes the service counts. A path that is not one of them is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `GET /v1/auth/challenge`.
    Challenge = 0,
    /// `POST /v1/auth/verify`.
    Verify = 1,
    /// `GET /v1/profile`.
    Profile = 2,
    /// `GET /health`.
    Health = 3,
    /// `GET /metrics`.
    Metrics = 4,
    /// `GET /openapi.json`.
    Openapi = 5,
    /// Anything else, including a path a client invented.
    Other = 6,
}

impl Route {
    /// Every route, in the order the counters are laid out.
    const ALL: [Self; 7] = [
        Self::Challenge,
        Self::Verify,
        Self::Profile,
        Self::Health,
        Self::Metrics,
        Self::Openapi,
        Self::Other,
    ];

    /// The route a request path belongs to.
    pub fn of(path: &str) -> Self {
        match path {
            "/v1/auth/challenge" => Self::Challenge,
            "/v1/auth/verify" => Self::Verify,
            "/v1/profile" => Self::Profile,
            "/health" => Self::Health,
            "/metrics" => Self::Metrics,
            "/openapi.json" => Self::Openapi,
            _ => Self::Other,
        }
    }

    /// The value of the `route` label.
    fn label(self) -> &'static str {
        match self {
            Self::Challenge => "challenge",
            Self::Verify => "verify",
            Self::Profile => "profile",
            Self::Health => "health",
            Self::Metrics => "metrics",
            Self::Openapi => "openapi",
            Self::Other => "other",
        }
    }

    /// Where the counters of this route start in the flat arrays.
    fn index(self) -> usize {
        self as usize
    }
}

/// How a request ended. Three classes and not the exact code: the code of a `4xx`
/// is chosen partly by the caller, and one series per code would be a table sized
/// by the clients.
#[derive(Debug, Clone, Copy)]
enum StatusClass {
    /// `2xx` and `3xx`.
    Ok = 0,
    /// `4xx`.
    Client = 1,
    /// `5xx` and anything unexpected.
    Server = 2,
}

impl StatusClass {
    /// Every class, in the order the counters are laid out.
    const ALL: [Self; 3] = [Self::Ok, Self::Client, Self::Server];

    /// The class of a status code.
    fn of(status: u16) -> Self {
        match status {
            200..=399 => Self::Ok,
            400..=499 => Self::Client,
            _ => Self::Server,
        }
    }

    /// The value of the `status` label.
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "2xx",
            Self::Client => "4xx",
            Self::Server => "5xx",
        }
    }

    /// Where the counters of this class start in the flat arrays.
    fn index(self) -> usize {
        self as usize
    }
}

/// How one profile fetch ended. A fetch that was tried twice counts once here,
/// by its final outcome, and once more in the retry counter. A fetch that the
/// rate limit window refused never left the process; it is counted anyway, since
/// what the caller sees is the same kind of answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The profile was read.
    Ok = 0,
    /// The key used up its rate limit window.
    RateLimited = 1,
    /// The player has no SkyBlock profile the service can read.
    Missing = 2,
    /// Anything else, from a rejected key to a connection that did not open.
    Failed = 3,
}

impl Outcome {
    /// Every outcome, in the order the counters are laid out.
    const ALL: [Self; 4] = [Self::Ok, Self::RateLimited, Self::Missing, Self::Failed];

    /// The value of the `outcome` label.
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::RateLimited => "rate_limited",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }

    /// Where the counter of this outcome lives.
    fn index(self) -> usize {
        self as usize
    }
}

/// The upper bounds of the latency histogram, in seconds. They follow the
/// budgets of this service: a cached profile is expected under ten milliseconds
/// and a cold one takes about a second, so both the fast path and the slow path
/// land in a bucket of their own.
const BUCKETS: [f64; 11] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

/// The routes, the status classes and the outcomes are fixed lists, so every
/// counter array has a size known at compile time.
const ROUTES: usize = Route::ALL.len();
const CLASSES: usize = StatusClass::ALL.len();
const OUTCOMES: usize = Outcome::ALL.len();
/// One slot per bound, plus the `+Inf` bucket.
const SLOTS: usize = BUCKETS.len() + 1;

/// The counters of the service. Every array is indexed by the enums above, so no
/// request can make this grow: the whole table is a few kilobytes built once.
pub struct Metrics {
    requests: [AtomicU64; ROUTES * CLASSES],
    latency: [AtomicU64; ROUTES * SLOTS],
    latency_micros: [AtomicU64; ROUTES],
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    fetches: [AtomicU64; OUTCOMES],
    retries: AtomicU64,
}

impl Metrics {
    /// Creates the table with every counter at zero.
    pub fn new() -> Self {
        Self {
            requests: std::array::from_fn(|_| AtomicU64::new(0)),
            latency: std::array::from_fn(|_| AtomicU64::new(0)),
            latency_micros: std::array::from_fn(|_| AtomicU64::new(0)),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            fetches: std::array::from_fn(|_| AtomicU64::new(0)),
            retries: AtomicU64::new(0),
        }
    }

    /// Records a request that has been answered.
    pub fn record(&self, route: Route, status: u16, took: Duration) {
        let class = StatusClass::of(status);
        self.requests[route.index() * CLASSES + class.index()].fetch_add(1, Ordering::Relaxed);
        self.observe(route, took);
    }

    /// Adds one observation to the histogram of a route.
    fn observe(&self, route: Route, took: Duration) {
        let seconds = took.as_secs_f64();
        let base = route.index() * SLOTS;
        // `le` means "less or equal", so the buckets are cumulative and an
        // observation is counted in every bucket it fits in.
        for (slot, bound) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.latency[base + slot].fetch_add(1, Ordering::Relaxed);
            }
        }
        // The last slot is `+Inf`, which every observation reaches.
        self.latency[base + BUCKETS.len()].fetch_add(1, Ordering::Relaxed);
        self.latency_micros[route.index()].fetch_add(took.as_micros() as u64, Ordering::Relaxed);
    }

    /// The cache answered, so no upstream call was needed.
    pub fn cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// The cache did not have the profile.
    pub fn cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// One fetch of a profile ended, after however many attempts it took.
    pub fn profile_fetch(&self, outcome: Outcome) {
        self.fetches[outcome.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// One extra attempt of a call that had already failed.
    pub fn upstream_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    /// Renders every counter in the text exposition format, version 0.0.4.
    ///
    /// The counters are read one at a time, so a scrape that overlaps a request
    /// may see a slightly older value for it. That is normal, and the
    /// alternative is a lock on the request path.
    pub fn render(&self, values: Values) -> String {
        let mut out = String::with_capacity(4096);
        self.render_requests(&mut out);
        self.render_latency(&mut out);
        self.render_totals(&mut out);
        self.render_gauges(&mut out, values);
        out
    }

    /// One series per route and status class, zero included: a series that
    /// disappears is worse than a zero, because a rate over it becomes a gap.
    fn render_requests(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP skyatlaz_http_requests_total Requests answered, by route and status class."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_http_requests_total counter");
        for route in Route::ALL {
            for class in StatusClass::ALL {
                let value =
                    self.requests[route.index() * CLASSES + class.index()].load(Ordering::Relaxed);
                let _ = writeln!(
                    out,
                    "skyatlaz_http_requests_total{{route=\"{}\",status=\"{}\"}} {value}",
                    route.label(),
                    class.label()
                );
            }
        }
    }

    /// The histogram of the time a request took, per route.
    fn render_latency(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP skyatlaz_http_request_duration_seconds How long a request took, by route."
        );
        let _ = writeln!(
            out,
            "# TYPE skyatlaz_http_request_duration_seconds histogram"
        );
        for route in Route::ALL {
            let base = route.index() * SLOTS;
            let label = route.label();
            for (slot, bound) in BUCKETS.iter().enumerate() {
                let value = self.latency[base + slot].load(Ordering::Relaxed);
                let _ = writeln!(
                    out,
                    "skyatlaz_http_request_duration_seconds_bucket{{route=\"{label}\",le=\"{bound}\"}} {value}"
                );
            }
            let count = self.latency[base + BUCKETS.len()].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "skyatlaz_http_request_duration_seconds_bucket{{route=\"{label}\",le=\"+Inf\"}} {count}"
            );
            let micros = self.latency_micros[route.index()].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "skyatlaz_http_request_duration_seconds_sum{{route=\"{label}\"}} {}",
                micros as f64 / 1_000_000.0
            );
            let _ = writeln!(
                out,
                "skyatlaz_http_request_duration_seconds_count{{route=\"{label}\"}} {count}"
            );
        }
    }

    /// The counters that are not about an HTTP request.
    fn render_totals(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP skyatlaz_profile_cache_hits_total Profiles answered from the cache."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_profile_cache_hits_total counter");
        let _ = writeln!(
            out,
            "skyatlaz_profile_cache_hits_total {}",
            self.cache_hits.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP skyatlaz_profile_cache_misses_total Profile requests the cache could not answer."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_profile_cache_misses_total counter");
        let _ = writeln!(
            out,
            "skyatlaz_profile_cache_misses_total {}",
            self.cache_misses.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            out,
            "# HELP skyatlaz_profile_fetches_total Profile fetches, by how they ended."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_profile_fetches_total counter");
        for outcome in Outcome::ALL {
            let _ = writeln!(
                out,
                "skyatlaz_profile_fetches_total{{outcome=\"{}\"}} {}",
                outcome.label(),
                self.fetches[outcome.index()].load(Ordering::Relaxed)
            );
        }

        let _ = writeln!(
            out,
            "# HELP skyatlaz_upstream_retries_total Upstream calls that needed a second attempt."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_upstream_retries_total counter");
        let _ = writeln!(
            out,
            "skyatlaz_upstream_retries_total {}",
            self.retries.load(Ordering::Relaxed)
        );
    }

    /// The values that are only true at the moment of the scrape.
    fn render_gauges(&self, out: &mut String, values: Values) {
        let _ = writeln!(
            out,
            "# HELP skyatlaz_profile_cache_entries Profiles the cache holds right now."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_profile_cache_entries gauge");
        let _ = writeln!(
            out,
            "skyatlaz_profile_cache_entries {}",
            values.cache_entries
        );

        let _ = writeln!(
            out,
            "# HELP skyatlaz_upstream_rate_limit The requests the key may do in its window."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_upstream_rate_limit gauge");
        let _ = writeln!(out, "skyatlaz_upstream_rate_limit {}", values.rate_limit);

        let _ = writeln!(
            out,
            "# HELP skyatlaz_upstream_rate_limit_remaining Requests left in the current window."
        );
        let _ = writeln!(out, "# TYPE skyatlaz_upstream_rate_limit_remaining gauge");
        let _ = writeln!(
            out,
            "skyatlaz_upstream_rate_limit_remaining {}",
            values.rate_limit_remaining
        );
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// The numbers that only exist at the moment of a scrape, read from the places
/// that own them rather than copied into a counter.
#[derive(Debug, Clone, Copy)]
pub struct Values {
    /// Profiles the cache holds right now.
    pub cache_entries: usize,
    /// The request budget the key advertises for its window.
    pub rate_limit: u32,
    /// What is left of that budget.
    pub rate_limit_remaining: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            cache_entries: 3,
            rate_limit: 300,
            rate_limit_remaining: 299,
        }
    }

    #[test]
    fn an_unknown_path_is_counted_as_other() {
        assert_eq!(Route::of("/v1/profile"), Route::Profile);
        assert_eq!(Route::of("/admin"), Route::Other);
        assert_eq!(Route::of(""), Route::Other);
    }

    #[test]
    fn a_status_is_put_in_one_of_three_classes() {
        assert_eq!(StatusClass::of(200).label(), "2xx");
        assert_eq!(StatusClass::of(404).label(), "4xx");
        assert_eq!(StatusClass::of(429).label(), "4xx");
        assert_eq!(StatusClass::of(503).label(), "5xx");
    }

    #[test]
    fn every_series_is_rendered_even_when_it_is_zero() {
        let rendered = Metrics::new().render(values());

        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("skyatlaz_http_requests_total{"))
                .count(),
            ROUTES * CLASSES
        );
        assert!(
            rendered.contains("skyatlaz_http_requests_total{route=\"profile\",status=\"2xx\"} 0")
        );
        assert!(rendered.contains("skyatlaz_profile_fetches_total{outcome=\"failed\"} 0"));
        assert!(rendered.contains("skyatlaz_profile_cache_entries 3"));
        assert!(rendered.contains("skyatlaz_upstream_rate_limit_remaining 299"));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn the_histogram_is_cumulative() {
        let metrics = Metrics::new();
        metrics.record(Route::Health, 200, Duration::from_millis(3));
        let rendered = metrics.render(values());

        // Three milliseconds land in 0.005 and in everything above it, never in
        // 0.001.
        assert!(rendered.contains("le=\"0.001\"} 0"));
        assert!(rendered.contains("le=\"0.005\"} 1"));
        assert!(rendered.contains("le=\"+Inf\"} 1"));
        assert!(rendered.contains("_count{route=\"health\"} 1"));
        assert!(rendered.contains("_sum{route=\"health\"} 0.003"));
        assert!(
            rendered.contains("skyatlaz_http_requests_total{route=\"health\",status=\"2xx\"} 1")
        );
    }

    #[test]
    fn a_client_cannot_create_a_series_per_path() {
        let metrics = Metrics::new();
        for index in 0..100 {
            metrics.record(
                Route::of(&format!("/invented/{index}")),
                404,
                Duration::from_millis(1),
            );
        }
        let rendered = metrics.render(values());

        assert!(!rendered.contains("/invented"), "a path reached the table");
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.starts_with("skyatlaz_http_requests_total{"))
                .count(),
            ROUTES * CLASSES,
            "the table grew with the paths"
        );
        assert!(
            rendered.contains("skyatlaz_http_requests_total{route=\"other\",status=\"4xx\"} 100")
        );
    }

    #[test]
    fn the_cache_and_the_fetches_are_counted_apart() {
        let metrics = Metrics::new();
        metrics.cache_hit();
        metrics.cache_miss();
        metrics.cache_hit();
        metrics.profile_fetch(Outcome::Ok);
        metrics.profile_fetch(Outcome::RateLimited);
        metrics.profile_fetch(Outcome::Missing);
        metrics.upstream_retry();
        let rendered = metrics.render(values());

        assert!(rendered.contains("skyatlaz_profile_cache_hits_total 2"));
        assert!(rendered.contains("skyatlaz_profile_cache_misses_total 1"));
        assert!(rendered.contains("skyatlaz_profile_fetches_total{outcome=\"ok\"} 1"));
        assert!(rendered.contains("skyatlaz_profile_fetches_total{outcome=\"rate_limited\"} 1"));
        assert!(rendered.contains("skyatlaz_profile_fetches_total{outcome=\"missing\"} 1"));
        assert!(rendered.contains("skyatlaz_profile_fetches_total{outcome=\"failed\"} 0"));
        assert!(rendered.contains("skyatlaz_upstream_retries_total 1"));
    }
}
