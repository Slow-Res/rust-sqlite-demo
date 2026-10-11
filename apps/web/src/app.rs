//! Router assembly: API routes, middleware stack, and optional static frontend.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::books;
use crate::error::AppError;
use crate::health;
use crate::loans;
use crate::members;
use crate::state::AppState;
use library_shared::{ApiError, ApiErrorBody};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT_BYTES: usize = 64 * 1024;
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Build the full application router.
pub fn build_router(state: AppState) -> Router {
    // Add a feature by merging its router here, e.g. `.merge(books::routes())`.
    let api = Router::new()
        .merge(health::routes())
        .merge(auth::routes())
        .merge(books::routes())
        .merge(members::routes())
        .merge(loans::routes())
        .fallback(api_not_found);

    let mut app = Router::new().nest("/api", api);

    if let Some(dir) = &state.config.frontend_dir {
        // Unknown paths serve index.html so client-side routes survive a refresh.
        let index = ServeFile::new(dir.join("index.html"));
        app = app.fallback_service(ServeDir::new(dir).fallback(index));
    }

    // `with_state` turns `Router<AppState>` into a plain `Router` once every
    // handler's state requirement is satisfied.
    let app = app.with_state(state);

    // `ServiceBuilder` lists layers outermost-first: a request passes through them top to bottom.
    app.layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid))
            .layer(PropagateRequestIdLayer::new(REQUEST_ID_HEADER))
            .layer(
                TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<_>| {
                    let request_id = request
                        .headers()
                        .get(REQUEST_ID_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    tracing::info_span!(
                        "request",
                        method = %request.method(),
                        path = %request.uri().path(),
                        request_id,
                    )
                }),
            )
            // Sits above the timeout layer so it also sees the 408 it produces.
            .layer(from_fn(json_error_bodies))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                REQUEST_TIMEOUT,
            ))
            .layer(CompressionLayer::new())
            .layer(security_header(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
            .layer(security_header(header::X_FRAME_OPTIONS, "DENY"))
            .layer(security_header(header::REFERRER_POLICY, "same-origin"))
            .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES)),
    )
}

fn security_header(name: HeaderName, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
}

/// The 405 (axum), 408 (timeout layer) and 413 (body-limit layer) responses are
/// produced outside our handlers with empty or plain-text bodies. Rewrite them
/// for `/api/*` so every API error has the JSON shape of spec §5.5. The status
/// is kept. `from_fn` wraps the rest of the stack in an async function: it can
/// look at the request, call `next.run(request)`, then adjust the response.
async fn json_error_bodies(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let response = next.run(request).await;
    if !is_api {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    match error_for_status(response.status()) {
        Some((code, message)) if !is_json => json_error(response.status(), code, message),
        _ => response,
    }
}

/// `(code, message)` for the statuses that [`json_error_bodies`] rewrites.
fn error_for_status(status: StatusCode) -> Option<(&'static str, &'static str)> {
    match status {
        StatusCode::METHOD_NOT_ALLOWED => Some(("method_not_allowed", "method not allowed")),
        StatusCode::REQUEST_TIMEOUT => Some(("request_timeout", "request timed out")),
        StatusCode::PAYLOAD_TOO_LARGE => Some(("payload_too_large", "request body too large")),
        _ => None,
    }
}

fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = ApiError {
        error: ApiErrorBody {
            code: code.to_owned(),
            message: message.to_owned(),
            fields: None,
        },
    };
    (status, Json(body)).into_response()
}

/// JSON 404 for any `/api/*` path that matches no route.
async fn api_not_found() -> AppError {
    AppError::NotFound("route")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;

    use super::*;

    /// Run `json_error_bodies` around a service that always answers `status`
    /// with an empty body, like the timeout layer does.
    async fn through_mapper(path: &str, status: StatusCode) -> Response {
        let app = Router::new()
            .fallback(move || async move { status })
            .layer(from_fn(json_error_bodies));
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        tower::ServiceExt::oneshot(app, request).await.unwrap()
    }

    #[tokio::test]
    async fn timeout_response_becomes_json() {
        let response = through_mapper("/api/books", StatusCode::REQUEST_TIMEOUT).await;
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "request_timeout");
    }

    #[tokio::test]
    async fn non_api_paths_and_other_statuses_are_left_alone() {
        let response = through_mapper("/books", StatusCode::REQUEST_TIMEOUT).await;
        assert!(response.headers().get(header::CONTENT_TYPE).is_none());
        let response = through_mapper("/api/books", StatusCode::BAD_GATEWAY).await;
        assert!(response.headers().get(header::CONTENT_TYPE).is_none());
    }
}
