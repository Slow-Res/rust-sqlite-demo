//! The `AuthUser` extractor: the single way a handler requires a login.

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;

use super::SESSION_COOKIE;
use super::service::AuthService;
use crate::error::AppError;

/// The logged-in librarian. Add `user: AuthUser` to a handler's arguments and
/// axum runs [`FromRequestParts`] before the handler; if it returns `Err`, the
/// handler never runs and that error becomes the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
}

// `S` is whatever state the router carries. The `FromRef` bound says "I can
// get an `AuthService` out of it", so this works with `AppState` (or any
// other state) without naming it.
impl<S> FromRequestParts<S> for AuthUser
where
    AuthService: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = AuthService::from_ref(state);
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(SESSION_COOKIE)
            .map(|cookie| cookie.value())
            .ok_or(AppError::Unauthorized)?;
        let user = auth.authenticate(token).await?;
        Ok(Self {
            id: user.id,
            username: user.username,
        })
    }
}
