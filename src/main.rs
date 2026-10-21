//! The SkyAtlaz service: a small HTTP proxy that lets the SkyAtlaz mod read the
//! Hypixel profile of the player that is using it, without every player having
//! to own a Hypixel developer key.
//!
//! The developer key lives in the environment of this service only. The mod
//! authenticates with its Minecraft profile key pair and then only ever asks
//! for its own profile.

mod auth;
mod cache;
mod config;
mod hypixel;
mod keys;
mod metrics;
mod ratelimit;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::auth::{AuthService, VerifyRequest, VerifyResponse};
use crate::cache::TtlCache;
use crate::config::Config;
use crate::hypixel::{HypixelClient, HypixelError, SlimProfile};
use crate::metrics::{Metrics, Route, Values};

/// The contract of the service, so a client can be written against it.
const OPENAPI: &str = include_str!("../openapi.json");

/// The largest request the service reads. A verify request carries two RSA
/// signatures and a public key, which is about one kilobyte, so anything close
/// to this limit is not a client the service has to serve.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// How long a request may take before the service gives up. The Hypixel client
/// gives up on its own after twenty seconds, so this only catches the rest.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The format version of the counters, as Prometheus expects it in the content
/// type of an exposition.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Everything the handlers need.
pub struct AppState {
    /// The authentication service.
    pub auth: AuthService,
    /// The client of the Hypixel API.
    pub hypixel: HypixelClient,
    /// The cached profiles.
    pub profiles: TtlCache<String, SlimProfile>,
    /// The counters served at `/metrics`.
    pub metrics: Arc<Metrics>,
}

/// An error that is turned into a JSON body.
#[derive(Debug)]
pub struct ApiError {
    /// The status code of the answer.
    pub status: StatusCode,
    /// A short machine readable code.
    pub code: &'static str,
    /// A human readable message.
    pub message: String,
    /// The seconds the client should wait, sent as `Retry-After`.
    pub retry_after: Option<u64>,
    /// Whether the answer asks the client to authenticate with a bearer token.
    pub www_authenticate: bool,
}

impl ApiError {
    /// The request did not carry a valid session token.
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: message.into(),
            retry_after: None,
            www_authenticate: false,
        }
    }

    /// The caller of the profile endpoint did not authenticate with a token.
    fn unauthorized_bearer(message: impl Into<String>) -> Self {
        Self {
            www_authenticate: true,
            ..Self::unauthorized(message)
        }
    }

    /// The player has no profile the service can read.
    fn profile_not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "profile_not_found",
            message: "the player has no SkyBlock profile the service can read".to_string(),
            retry_after: None,
            www_authenticate: false,
        }
    }

    /// The address asked for too many challenges in a minute.
    fn too_many_requests() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "too_many_requests",
            message: "too many challenges from this address".to_string(),
            // The window is a minute, so that is the honest wait.
            retry_after: Some(60),
            www_authenticate: false,
        }
    }

    /// The Hypixel API could not be read.
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_error",
            message: message.into(),
            retry_after: None,
            www_authenticate: false,
        }
    }

    /// The key used up its rate limit, so the client should come back later.
    fn unavailable(wait: Duration) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "rate_limited",
            message: "the service used up the Hypixel rate limit, retry later".to_string(),
            retry_after: Some(wait.as_secs().max(1)),
            www_authenticate: false,
        }
    }

    /// Turns a client failure into an answer the mod can act on.
    fn from_hypixel(error: HypixelError) -> Self {
        match error {
            HypixelError::RateLimited(wait) => Self::unavailable(wait),
            HypixelError::Missing(field) => {
                tracing::debug!(field, "the player has no profile the service can read");
                Self::profile_not_found()
            }
            // The cause of an upstream failure is for the logs: a client cannot
            // do anything with it and it may describe the state of the key.
            other => {
                tracing::warn!(error = %other, "the Hypixel API could not be read");
                Self::upstream("the Hypixel API could not be read")
            }
        }
    }
}

/// The body of an error answer.
#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: &'a str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorBody {
                error: self.code,
                message: &self.message,
            }),
        )
            .into_response();

        let headers = response.headers_mut();
        if let Some(seconds) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if self.www_authenticate {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// The answer of the challenge endpoint.
#[derive(Serialize)]
struct ChallengeResponse {
    nonce: String,
    expires_at: i64,
}

/// The health endpoint.
async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let (limit, remaining) = state.hypixel.rate_limit();
    Json(serde_json::json!({
        "ok": true,
        "service": "skyatlaz-service",
        "version": env!("CARGO_PKG_VERSION"),
        "rate_limit": { "limit": limit, "remaining": remaining },
        "cache": { "entries": state.profiles.len() },
    }))
}

/// The counters, in the Prometheus text format.
///
/// The numbers that only exist at scrape time are read from where they live
/// instead of being copied into a counter: the cache knows how many entries it
/// holds and the upstream client knows what its key may still do.
async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (limit, remaining) = state.hypixel.rate_limit();
    let body = state.metrics.render(Values {
        cache_entries: state.profiles.len(),
        rate_limit: limit,
        rate_limit_remaining: remaining,
    });
    ([(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)], body)
}

/// Counts a request once it has been answered, whatever the answer was.
///
/// The path is picked by the caller, so it is resolved through the closed set of
/// names of `metrics::Route` and never used as a label value directly: a table
/// whose size a client chooses is a memory leak with a nice format.
async fn count_request(
    State(metrics): State<Arc<Metrics>>,
    request: Request,
    next: Next,
) -> Response {
    let route = Route::of(request.uri().path());
    let started = Instant::now();
    let response = next.run(request).await;
    metrics.record(route, response.status().as_u16(), started.elapsed());
    response
}

/// The contract of the service.
async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI)
}

/// Answers a path that does not exist with the same error shape as everything
/// else, so a client never has to parse an empty body.
async fn not_found() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "not_found",
        message: "no route matches that path and method".to_string(),
        retry_after: None,
        www_authenticate: false,
    }
}

/// Hands out a nonce the mod has to sign.
async fn challenge(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<ChallengeResponse>, ApiError> {
    if !state.auth.allow_challenge(client_address(peer, &headers)) {
        return Err(ApiError::too_many_requests());
    }
    let (nonce, expires_at) = state.auth.issue_challenge();
    Ok(Json(ChallengeResponse { nonce, expires_at }))
}

/// Reads the address a request comes from, so the challenge limit applies to the
/// caller and not to the proxy in front of it.
///
/// Behind Caddy every connection comes from loopback, so that is the only case
/// where `X-Forwarded-For` is read: the proxy appends the address it saw, which
/// is the last entry. For any other peer the socket address is the truth and the
/// header is ignored, so a client cannot pick its own bucket.
fn client_address(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let peer = peer.ip();
    if !peer.is_loopback() {
        return peer;
    }

    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

/// Verifies the signed challenge and hands out a session token.
async fn verify(
    State(state): State<Arc<AppState>>,
    Json(request): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, ApiError> {
    match state.auth.verify(&request) {
        Ok(response) => Ok(Json(response)),
        Err(error) => Err(ApiError::unauthorized(error.to_string())),
    }
}

/// Returns the profile of the caller.
async fn profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SlimProfile>, ApiError> {
    let token = bearer_token(&headers)
        .ok_or_else(|| ApiError::unauthorized_bearer("missing bearer token"))?;
    let uuid = state
        .auth
        .validate_token(token)
        .map_err(|error| ApiError::unauthorized_bearer(error.to_string()))?;

    if let Some(cached) = state.profiles.get(&uuid) {
        state.metrics.cache_hit();
        return Ok(Json(cached));
    }
    state.metrics.cache_miss();

    let profile = state
        .hypixel
        .fetch_profile(&uuid)
        .await
        .map_err(ApiError::from_hypixel)?;
    state.profiles.insert(uuid, profile.clone());
    Ok(Json(profile))
}

/// Reads the bearer token out of the headers. The scheme is case insensitive
/// and an empty token is not a token.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Builds the router. Kept apart from `main` so the tests can exercise the real
/// stack, with its layers, without binding a port.
fn app(state: Arc<AppState>) -> Router {
    // Everything under /v1 answers with data that belongs to one player, so no
    // cache, proxy or browser may store it.
    let api = Router::new()
        .route("/v1/auth/challenge", get(challenge))
        .route("/v1/auth/verify", post(verify))
        .route("/v1/profile", get(profile))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::PRAGMA,
            HeaderValue::from_static("no-cache"),
        ));

    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi))
        .merge(api)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        // Outermost, so a request that the timeout layer cuts short is counted
        // with the status the client actually saw.
        .layer(from_fn_with_state(
            Arc::clone(&state.metrics),
            count_request,
        ))
        .with_state(state)
}

/// Resolves when the process is asked to stop, either by Ctrl-C or by the
/// SIGTERM that systemd sends.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("the ctrl-c handler is installed");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("the SIGTERM handler is installed")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("a shutdown was requested, finishing the open requests");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A local .env is a convenience for development. Variables that are already
    // set win, so a real environment is never overridden.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let bind: SocketAddr = config.bind.parse().map_err(|error| {
        format!(
            "the bind address '{}' is not a valid address: {error}",
            config.bind
        )
    })?;

    let metrics = Arc::new(Metrics::new());
    let state = Arc::new(AppState {
        auth: AuthService::new(config.token_secret.clone(), config.token_ttl),
        hypixel: HypixelClient::new(&config.hypixel_api_key, &config.hypixel_base)?
            .with_metrics(Arc::clone(&metrics)),
        profiles: TtlCache::new(config.profile_ttl, config.profile_cache_entries),
        metrics,
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(
        %bind,
        profile_ttl_seconds = config.profile_ttl.as_secs(),
        token_ttl_seconds = config.token_ttl.as_secs(),
        cache_entries = config.profile_cache_entries,
        "skyatlaz-service is listening"
    );

    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::hypixel::MiningProfile;

    fn state() -> Arc<AppState> {
        let metrics = Arc::new(Metrics::new());
        Arc::new(AppState {
            auth: AuthService::new(
                b"a-test-secret-that-is-long-enough".to_vec(),
                Duration::from_secs(3600),
            ),
            hypixel: HypixelClient::new("a-test-key", "http://127.0.0.1:9")
                .expect("the client is built")
                .with_metrics(Arc::clone(&metrics)),
            profiles: TtlCache::new(Duration::from_secs(60), 8),
            metrics,
        })
    }

    /// Sends a request through the real router, layers included.
    async fn send(request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
        send_with(&app(state()), request).await
    }

    /// Sends a request through an existing router, so a test can reuse the
    /// state of one service across several requests.
    async fn send_with(
        router: &Router,
        request: Request<Body>,
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let (status, headers, body) = send_raw(router, request).await;
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, headers, json)
    }

    /// Sends a request and returns the body as text, for the routes that do not
    /// answer with JSON.
    async fn send_text(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let (status, headers, body) = send_raw(router, request).await;
        (
            status,
            headers,
            String::from_utf8(body).expect("the body is text"),
        )
    }

    async fn send_raw(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        (status, headers, body.to_vec())
    }

    /// A profile the tests can put in the cache without an upstream call.
    fn profile(uuid: &str) -> SlimProfile {
        SlimProfile {
            uuid: uuid.to_string(),
            name: None,
            mining: MiningProfile::default(),
            dungeons: serde_json::Value::Null,
            currencies: serde_json::Value::Null,
            fetched_at: 0,
        }
    }

    /// A request that carries a bearer token.
    fn authorized(uri: &str, token: &str) -> Request<Body> {
        let mut request = get(uri);
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("the token is a header value"),
        );
        request
    }

    fn get(uri: &str) -> Request<Body> {
        let mut request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("the request is valid");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:1234"
                .parse::<SocketAddr>()
                .expect("the address is valid"),
        ));
        request
    }

    fn contract() -> serde_json::Value {
        serde_json::from_str(OPENAPI).expect("the contract is valid json")
    }

    #[test]
    fn the_contract_lists_every_endpoint() {
        let contract = contract();
        let paths = contract["paths"]
            .as_object()
            .expect("the contract has paths");

        for path in [
            "/health",
            "/metrics",
            "/openapi.json",
            "/v1/auth/challenge",
            "/v1/auth/verify",
            "/v1/profile",
        ] {
            assert!(
                paths.contains_key(path),
                "the contract does not mention {path}"
            );
        }
    }

    #[test]
    fn the_contract_describes_the_profile() {
        let contract = contract();
        let schemas = &contract["components"]["schemas"];

        assert!(schemas["SlimProfile"]["properties"]["mining"].is_object());
        assert!(schemas["MiningProfile"]["properties"]["perks"].is_object());
        assert!(schemas["MiningProfile"]["properties"]["toggles"].is_object());
        assert!(schemas["MiningProfile"]["properties"]["powder"].is_object());
        assert!(schemas["MiningProfile"]["properties"]["ability"].is_object());
    }

    #[test]
    fn the_contract_mentions_the_rate_limit_answer() {
        let contract = contract();
        let profile = &contract["paths"]["/v1/profile"]["get"]["responses"];

        assert!(profile["404"].is_object(), "the contract misses a 404");
        assert!(profile["503"].is_object(), "the contract misses a 503");
    }

    #[test]
    fn the_contract_mentions_the_upstream_failure_answer() {
        let contract = contract();
        let profile = &contract["paths"]["/v1/profile"]["get"]["responses"];

        assert!(profile["502"].is_object(), "the contract misses a 502");
        assert_eq!(
            profile["502"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Error",
            "a 502 has to carry the same error body as the other failures"
        );
    }

    #[test]
    fn the_contract_mentions_the_challenge_limit() {
        let contract = contract();
        let challenge = &contract["paths"]["/v1/auth/challenge"]["get"]["responses"];

        assert!(challenge["429"].is_object(), "the contract misses a 429");
    }

    #[tokio::test]
    async fn the_health_endpoint_reports_the_service() {
        let (status, headers, body) = send(get("/health")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["service"], "skyatlaz-service");
        assert_eq!(
            headers
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
    }

    #[tokio::test]
    async fn the_metrics_endpoint_is_a_prometheus_document() {
        let state = state();
        let router = app(Arc::clone(&state));
        let (status, _, _) = send_with(&router, get("/health")).await;
        assert_eq!(status, StatusCode::OK);

        let (status, headers, body) = send_text(&router, get("/metrics")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
        assert!(
            body.contains("skyatlaz_http_requests_total{route=\"health\",status=\"2xx\"} 1"),
            "the health request was not counted: {body}"
        );
        assert!(body.contains("skyatlaz_profile_cache_entries 0"), "{body}");
        assert!(
            body.contains("skyatlaz_upstream_rate_limit_remaining 0"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn an_invented_path_does_not_create_a_series_of_its_own() {
        let state = state();
        let router = app(Arc::clone(&state));
        for index in 0..40 {
            let (status, _, _) = send_with(&router, get(&format!("/invented/{index}"))).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        let (_, _, body) = send_text(&router, get("/metrics")).await;

        assert!(!body.contains("/invented"), "a path reached the metrics");
        // Seven routes and three status classes, so twenty one series whatever
        // the callers do.
        assert_eq!(
            body.lines()
                .filter(|line| line.starts_with("skyatlaz_http_requests_total{"))
                .count(),
            21,
            "the table grew with the paths"
        );
        assert!(
            body.contains("skyatlaz_http_requests_total{route=\"other\",status=\"4xx\"} 40"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_cache_hit_and_a_missed_fetch_are_counted_apart() {
        let state = state();
        let router = app(Arc::clone(&state));

        // A player whose profile is already in the cache is answered without
        // touching the upstream.
        let cached = uuid::Uuid::new_v4();
        let token = state
            .auth
            .issue_token(&cached, crate::auth::now_millis() + 60_000);
        state
            .profiles
            .insert(cached.to_string(), profile(&cached.to_string()));
        let (status, _, _) = send_with(&router, authorized("/v1/profile", &token)).await;
        assert_eq!(status, StatusCode::OK);

        // A player whose profile is not cached needs the upstream, and this test
        // points the client at a port nothing listens on.
        let uncached = uuid::Uuid::new_v4();
        let token = state
            .auth
            .issue_token(&uncached, crate::auth::now_millis() + 60_000);
        let (status, _, _) = send_with(&router, authorized("/v1/profile", &token)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let (_, _, body) = send_text(&router, get("/metrics")).await;

        assert!(
            body.contains("skyatlaz_profile_cache_hits_total 1"),
            "{body}"
        );
        assert!(
            body.contains("skyatlaz_profile_cache_misses_total 1"),
            "{body}"
        );
        assert!(
            body.contains("skyatlaz_profile_fetches_total{outcome=\"failed\"} 1"),
            "{body}"
        );
        // The connection was refused, which is worth a second attempt.
        assert!(body.contains("skyatlaz_upstream_retries_total 1"), "{body}");
    }

    #[tokio::test]
    async fn the_contract_is_served_as_json() {
        let (status, headers, body) = send(get("/openapi.json")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(body["openapi"], "3.1.0");
    }

    #[tokio::test]
    async fn a_challenge_has_a_nonce() {
        let (status, _, body) = send(get("/v1/auth/challenge")).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body["nonce"]
                .as_str()
                .is_some_and(|nonce| !nonce.is_empty()),
            "the challenge has no nonce: {body}"
        );
        assert!(body["expires_at"].as_i64().is_some_and(|at| at > 0));
    }

    #[tokio::test]
    async fn one_address_cannot_flood_the_challenge_endpoint() {
        let router = app(state());
        let peer = SocketAddr::from(([10, 0, 0, 7], 5000));

        let mut throttled = None;
        for _ in 0..1_000 {
            let mut request = get("/v1/auth/challenge");
            request.extensions_mut().insert(ConnectInfo(peer));
            let (status, headers, _) = send_with(&router, request).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                throttled = Some(headers);
                break;
            }
        }

        let headers = throttled.expect("the address is throttled at some point");
        assert_eq!(
            headers
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("60")
        );
    }

    #[tokio::test]
    async fn the_challenge_limit_is_per_address_behind_the_proxy() {
        let router = app(state());
        let peer: SocketAddr = "127.0.0.1:5000".parse().expect("the address is valid");

        async fn challenge_as(router: &Router, peer: SocketAddr, forwarded: &str) -> StatusCode {
            let mut request = get("/v1/auth/challenge");
            request.extensions_mut().insert(ConnectInfo(peer));
            request.headers_mut().insert(
                "x-forwarded-for",
                forwarded.parse().expect("a valid header value"),
            );
            send_with(router, request).await.0
        }

        // One player uses up the window of that address...
        let mut last = StatusCode::OK;
        for _ in 0..31 {
            last = challenge_as(&router, peer, "203.0.113.9").await;
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);

        // ...while the next player behind the same proxy still gets a nonce.
        assert_eq!(
            challenge_as(&router, peer, "203.0.113.10").await,
            StatusCode::OK
        );
    }

    #[test]
    fn the_forwarded_address_is_only_trusted_from_the_local_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.9, 198.51.100.7"
                .parse()
                .expect("a valid header value"),
        );

        // A remote peer is what it says it is, whatever the header claims.
        let remote: SocketAddr = "10.0.0.7:5000".parse().expect("the address is valid");
        assert_eq!(
            client_address(remote, &headers),
            IpAddr::from([10, 0, 0, 7])
        );

        // The proxy appends the address it saw, so the last entry wins.
        let proxy: SocketAddr = "127.0.0.1:5000".parse().expect("the address is valid");
        assert_eq!(
            client_address(proxy, &headers),
            IpAddr::from([198, 51, 100, 7])
        );

        // Without the header the proxy address is the only truth we have.
        assert_eq!(client_address(proxy, &HeaderMap::new()), proxy.ip());
    }

    #[tokio::test]
    async fn the_profile_endpoint_requires_a_token() {
        let (status, headers, body) = send(get("/v1/profile")).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(
            headers
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer")
        );
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
    }

    #[tokio::test]
    async fn a_token_that_was_not_issued_here_is_rejected() {
        let request = Request::builder()
            .uri("/v1/profile")
            .header(header::AUTHORIZATION, "Bearer not-a-token")
            .body(Body::empty())
            .expect("the request is valid");

        let (status, _, body) = send(request).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn an_unknown_route_answers_with_json() {
        let (status, _, body) = send(get("/v1/unknown")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused() {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/auth/verify")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(vec![b'a'; MAX_REQUEST_BYTES + 1]))
            .expect("the request is valid");

        let (status, _, _) = send(request).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_verify_request_with_an_unknown_nonce_is_unauthorized() {
        let payload = serde_json::json!({
            "uuid": "01234567-89ab-cdef-0123-456789abcdef",
            "public_key": "",
            "public_key_signature": "",
            "expires_at": 0,
            "nonce": "bm90LWEtbm9uY2U=",
            "signature": "",
        });
        let request = Request::builder()
            .method("POST")
            .uri("/v1/auth/verify")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .expect("the request is valid");

        let (status, _, body) = send(request).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer  a-token".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("a-token"));

        headers.insert(header::AUTHORIZATION, "bearer a-token".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("a-token"));

        headers.insert(header::AUTHORIZATION, "Basic a-token".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);

        headers.insert(header::AUTHORIZATION, "Bearer   ".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }

    /// The whole router against a broken or slow upstream.
    ///
    /// `hypixel.rs` covers what the client does with a bad answer. What was
    /// missing is the same answer seen by a client of this service: a real
    /// `oneshot` request through every layer, with the upstream on a loopback
    /// port, has to end as a `502 upstream_error` with a JSON body and must not
    /// turn a broken body into a second upstream call.
    ///
    /// The block is self contained on purpose: it only adds a helper and tests,
    /// so it does not collide with work on the cache or the profile handler.
    mod upstream_failure_tests {
        use super::*;

        use std::sync::atomic::{AtomicUsize, Ordering};

        use serde_json::json;

        use crate::hypixel::UPSTREAM_TIMEOUT;

        /// The uuid the router derives from the token and asks the upstream for.
        const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";

        /// What the fake Hypixel does before it answers.
        #[derive(Clone)]
        enum Behaviour {
            /// A 200 whose body is not JSON at all.
            NotJson,
            /// A 200 with the first bytes of a JSON document and no end.
            Truncated,
            /// A 200 with a well formed answer that does not contain the player.
            MissingMember,
            /// A 404 with a well formed profile body: the upstream path moved.
            MissingResource,
            /// A well formed answer, answered at once.
            Valid,
            /// A well formed answer, after a stall.
            Slow(Duration),
        }

        /// A valid profile answer for `UUID`, so a successful fetch parses.
        fn valid_profile() -> String {
            json!({
                "success": true,
                "profiles": [{
                    "selected": true,
                    "members": {
                        "0123456789abcdef0123456789abcdef": {
                            "currencies": { "coin": 42 }
                        }
                    }
                }]
            })
            .to_string()
        }

        /// Starts the fake upstream and returns its base url and the number of
        /// requests that reached it.
        async fn fake_upstream(behaviour: Behaviour) -> (String, Arc<AtomicUsize>) {
            type Script = (Behaviour, Arc<AtomicUsize>);

            async fn answer(State((behaviour, calls)): State<Script>) -> (StatusCode, String) {
                calls.fetch_add(1, Ordering::SeqCst);
                match behaviour {
                    Behaviour::NotJson => (StatusCode::OK, "not json".to_string()),
                    Behaviour::Truncated => (
                        StatusCode::OK,
                        "{\"success\":true,\"profiles\":[".to_string(),
                    ),
                    Behaviour::MissingMember => (
                        StatusCode::OK,
                        json!({
                            "success": true,
                            "profiles": [{ "selected": true, "members": {} }]
                        })
                        .to_string(),
                    ),
                    // The body parses, but the status says the endpoint is not
                    // there. That is a broken deployment, not a missing player.
                    Behaviour::MissingResource => (StatusCode::NOT_FOUND, valid_profile()),
                    Behaviour::Valid => (StatusCode::OK, valid_profile()),
                    Behaviour::Slow(delay) => {
                        tokio::time::sleep(delay).await;
                        (StatusCode::OK, valid_profile())
                    }
                }
            }

            let calls = Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                .route("/v2/skyblock/profiles", axum::routing::get(answer))
                .with_state((behaviour, calls.clone()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the fake upstream binds");
            let address = listener
                .local_addr()
                .expect("the fake upstream has an address");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            (format!("http://{address}"), calls)
        }

        /// A service whose client points at the fake upstream instead of Hypixel.
        ///
        /// Only the timeout is changed, and only here: `HypixelClient::new` keeps
        /// the 20 s budget for everything that is not a test.
        fn state_with_upstream(base: &str, timeout: Duration) -> Arc<AppState> {
            let metrics = Arc::new(Metrics::new());
            Arc::new(AppState {
                auth: AuthService::new(
                    b"a-test-secret-that-is-long-enough".to_vec(),
                    Duration::from_secs(3600),
                ),
                hypixel: HypixelClient::with_timeout("a-test-key", base, timeout)
                    .expect("the client is built")
                    .with_metrics(Arc::clone(&metrics)),
                profiles: TtlCache::new(Duration::from_secs(60), 8),
                metrics,
            })
        }

        /// A token that makes the router read the profile of `UUID`.
        fn token(state: &AppState) -> String {
            let uuid = uuid::Uuid::parse_str(UUID).expect("the uuid is valid");
            state
                .auth
                .issue_token(&uuid, crate::auth::now_millis() + 60_000)
        }

        /// Drives one request through the real router and checks the answer is a
        /// bad gateway with a JSON error body, that the upstream was called the
        /// expected number of times, and that nothing broken was cached.
        async fn assert_bad_gateway(
            behaviour: Behaviour,
            client_timeout: Duration,
            expected_calls: usize,
        ) {
            let (base, calls) = fake_upstream(behaviour).await;
            let state = state_with_upstream(&base, client_timeout);
            let router = app(Arc::clone(&state));

            let (status, headers, body) =
                send_raw(&router, authorized("/v1/profile", &token(&state))).await;

            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(
                headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/json"),
                "the client has to be able to parse the error body"
            );
            let body: serde_json::Value =
                serde_json::from_slice(&body).expect("the error body is JSON");
            assert_eq!(body["error"], "upstream_error");
            assert_eq!(
                body["message"].as_str(),
                Some("the Hypixel API could not be read")
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "the upstream call count changed"
            );
            assert_eq!(state.profiles.len(), 0, "a failed fetch reached the cache");
        }

        #[tokio::test]
        async fn a_non_json_upstream_answers_502_and_is_not_retried() {
            // A decode failure is not a 5xx: the answer is broken, not early, so
            // a second call would only spend the key again.
            assert_bad_gateway(Behaviour::NotJson, UPSTREAM_TIMEOUT, 1).await;
        }

        #[tokio::test]
        async fn a_truncated_body_answers_502_and_is_not_retried() {
            // The upstream answers well within the 20 s budget, so the test is
            // about the parse and not about the clock.
            assert_bad_gateway(Behaviour::Truncated, UPSTREAM_TIMEOUT, 1).await;
        }

        #[tokio::test]
        async fn an_upstream_404_with_a_valid_body_answers_502_and_is_not_retried() {
            // A parseable body under a non-success status means the upstream
            // endpoint is wrong, not that the player has no profile, so the
            // client must not turn it into a 404 of its own.
            assert_bad_gateway(Behaviour::MissingResource, UPSTREAM_TIMEOUT, 1).await;
        }

        #[tokio::test]
        async fn an_upstream_that_stalls_past_the_timeout_answers_502() {
            // A few dozen milliseconds model a stall that outlives the client
            // timeout and still ends well inside the 30 s request timeout: the
            // client gives up twice and the router reports a bad gateway. Both
            // values are fractions of the production budget, so no real timeout
            // moves.
            let client_timeout = UPSTREAM_TIMEOUT / 800; // 25 ms
            let upstream_delay = UPSTREAM_TIMEOUT / 400; // 50 ms
            assert!(
                client_timeout < upstream_delay,
                "the test would not time out"
            );

            // A timeout is the one case the documented rules retry once, see
            // `docs/ARCHITECTURE.md` section 3.3; the answer is still a 502.
            assert_bad_gateway(Behaviour::Slow(upstream_delay), client_timeout, 2).await;
        }

        #[tokio::test]
        async fn a_valid_profile_without_the_member_is_404_and_not_retried() {
            // Not every bad answer is an upstream failure: a well formed answer
            // for a player without a profile is a 404 the mod can act on, and a
            // second call would not produce one either.
            let (base, calls) = fake_upstream(Behaviour::MissingMember).await;
            let state = state_with_upstream(&base, UPSTREAM_TIMEOUT);
            let router = app(Arc::clone(&state));

            let (status, _, body) =
                send_with(&router, authorized("/v1/profile", &token(&state))).await;

            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(body["error"], "profile_not_found");
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()),
                "the error has no message: {body}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "a missing profile was retried"
            );
            assert_eq!(state.profiles.len(), 0, "a missing profile was cached");
        }

        #[tokio::test]
        async fn a_valid_upstream_answers_the_profile_through_the_router() {
            // The control for the tests above: the fake and the token work, so a
            // 502 there is the failure path and not a broken fixture.
            let (base, calls) = fake_upstream(Behaviour::Valid).await;
            let state = state_with_upstream(&base, UPSTREAM_TIMEOUT);
            let router = app(Arc::clone(&state));

            let (status, _, body) =
                send_with(&router, authorized("/v1/profile", &token(&state))).await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["uuid"], UUID);
            assert_eq!(body["currencies"]["coin"], 42);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.profiles.len(), 1, "the profile was not cached");
        }
    }
}
