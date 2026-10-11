//! Request extractors whose rejections become [`AppError`], so malformed input
//! produces the same JSON error shape as everything else.
//!
//! An *extractor* is a handler argument that axum builds from the request
//! (body, query string, path). The derives below wrap axum's built-in
//! extractors and swap their plain-text rejections for `AppError`.

use axum::extract::{FromRequest, FromRequestParts};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::error::AppError;

/// JSON request body, and also a JSON response wrapper.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(AppError))]
pub struct ApiJson<T>(pub T);

impl<T: Serialize> IntoResponse for ApiJson<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Query string (`?page=2`).
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(AppError))]
pub struct ApiQuery<T>(pub T);

/// Path parameters (`/books/{id}`).
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(AppError))]
pub struct ApiPath<T>(pub T);
