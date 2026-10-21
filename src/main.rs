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
mod ratelimit;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use crate::auth::{AuthService, VerifyRequest, VerifyResponse};
use crate::cache::TtlCache;
use crate::config::Config;
use crate::hypixel::{HypixelClient, SlimProfile};

/// The contract of the service, so a client can be written against it.
const OPENAPI: &str = include_str!("../openapi.json");

/// Everything the handlers need.
pub struct AppState {
    /// The authentication service.
    pub auth: AuthService,
    /// The client of the Hypixel API.
    pub hypixel: HypixelClient,
    /// The cached profiles.
    pub profiles: TtlCache<String, SlimProfile>,
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
}

impl ApiError {
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: message.into(),
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_error",
            message: message.into(),
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
        (
            self.status,
            Json(ErrorBody {
                error: self.code,
                message: &self.message,
            }),
        )
            .into_response()
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
    }))
}

/// The contract of the service.
async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI)
}

/// Hands out a nonce the mod has to sign.
async fn challenge(State(state): State<Arc<AppState>>) -> Json<ChallengeResponse> {
    let (nonce, expires_at) = state.auth.issue_challenge();
    Json(ChallengeResponse { nonce, expires_at })
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
    let token = bearer_token(&headers).ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
    let uuid = state
        .auth
        .validate_token(token)
        .map_err(|error| ApiError::unauthorized(error.to_string()))?;

    if let Some(cached) = state.profiles.get(&uuid) {
        return Ok(Json(cached));
    }

    let profile = state
        .hypixel
        .fetch_profile(&uuid)
        .await
        .map_err(|error| ApiError::bad_gateway(error.to_string()))?;
    state.profiles.insert(uuid, profile.clone());
    Ok(Json(profile))
}

/// Reads the bearer token out of the headers.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::trim)
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
    let bind: SocketAddr = config.bind.parse()?;

    let state = Arc::new(AppState {
        auth: AuthService::new(config.token_secret.clone(), config.token_ttl),
        hypixel: HypixelClient::new(&config.hypixel_api_key, &config.hypixel_base)?,
        profiles: TtlCache::new(config.profile_ttl),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/v1/auth/challenge", get(challenge))
        .route("/v1/auth/verify", post(verify))
        .route("/v1/profile", get(profile))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "skyatlaz-service is listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> serde_json::Value {
        serde_json::from_str(OPENAPI).expect("the contract is valid json")
    }

    #[test]
    fn the_contract_lists_every_endpoint() {
        let contract = contract();
        let paths = contract["paths"].as_object().expect("the contract has paths");

        for path in [
            "/health",
            "/openapi.json",
            "/v1/auth/challenge",
            "/v1/auth/verify",
            "/v1/profile",
        ] {
            assert!(paths.contains_key(path), "the contract does not mention {path}");
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
}
