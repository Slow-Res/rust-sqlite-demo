//! HTTP layer for `/api/auth`: login, logout, me.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use cookie::time::Duration as CookieDuration;
use library_shared::{CurrentUser, LoginRequest};

use super::SESSION_COOKIE;
use super::extractor::AuthUser;
use super::service::AuthService;
use crate::config::Config;
use crate::error::AppError;
use crate::extract::ApiJson;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
}

// Handler arguments are extractors. `State<T>` pulls `T` out of `AppState`
// through the `FromRef` impls; `CookieJar` reads the request's cookies and,
// when returned, writes `Set-Cookie` headers on the response. The body
// extractor must come last because it consumes the request body.
async fn login(
    State(auth): State<AuthService>,
    State(config): State<Arc<Config>>,
    jar: CookieJar,
    ApiJson(request): ApiJson<LoginRequest>,
) -> Result<(CookieJar, ApiJson<CurrentUser>), AppError> {
    let session = auth.login(request).await?;
    let cookie = Cookie::build((SESSION_COOKIE, session.token))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(config.cookie_secure)
        .max_age(CookieDuration::seconds(auth.session_ttl().num_seconds()))
        .build();
    Ok((jar.add(cookie), ApiJson(session.user)))
}

async fn logout(
    State(auth): State<AuthService>,
    user: AuthUser,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), AppError> {
    // `AuthUser` already proved the cookie exists and is valid.
    let cookie = jar.get(SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
    auth.logout(cookie.value()).await?;
    tracing::debug!(user_id = user.id, "logged out");
    let removal = Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .removal()
        .build();
    Ok((jar.add(removal), StatusCode::NO_CONTENT))
}

async fn me(user: AuthUser) -> ApiJson<CurrentUser> {
    ApiJson(CurrentUser {
        id: user.id,
        username: user.username,
    })
}
