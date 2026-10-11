//! Application error type and its HTTP mapping (spec §5.5).
//!
//! Every error response has the same JSON shape (`library_shared::ApiError`).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use library_shared::{ApiError, ApiErrorBody};

use crate::db::RepoError;

/// Per-field validation messages, keyed by DTO field name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FieldErrors(BTreeMap<String, String>);

impl FieldErrors {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message for `field`; the first message for a field wins.
    pub fn add(&mut self, field: &str, message: &str) {
        self.0
            .entry(field.to_owned())
            .or_insert_with(|| message.to_owned());
    }

    /// `(field, message)` pairs in field-name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `Ok(())` if nothing was recorded, otherwise a `Validation` error.
    pub fn into_result(self) -> Result<(), AppError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(AppError::Validation(self))
        }
    }
}

/// What services return. `thiserror` derives `Display`/`Error` from the
/// `#[error(...)]` attributes.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("validation failed")]
    Validation(FieldErrors),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    /// Failed login. Same status and code as `Unauthorized`, but with the
    /// message spec §5.7 requires (identical for unknown user and wrong password).
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    /// Anything unexpected. The inner error is logged, never sent to the client.
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

/// Lets services use `?` on repository calls. A `Conflict` that reaches this
/// conversion was not handled by the service (which should map known unique
/// constraints to a specific 409), so it is treated as an internal bug.
impl From<RepoError> for AppError {
    fn from(err: RepoError) -> Self {
        Self::Internal(err.into())
    }
}

// Every extractor rejection maps to 400 `bad_request` (spec §5.5). An oversized
// body is rejected earlier with 413 by the body-limit layer, before extraction.
impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl From<QueryRejection> for AppError {
    fn from(rejection: QueryRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl From<PathRejection> for AppError {
    fn from(rejection: PathRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized | Self::InvalidCredentials => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict { .. } => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn body(&self) -> ApiErrorBody {
        let (code, message, fields) = match self {
            Self::Validation(fields) => (
                "validation_error",
                "Some fields are invalid".to_owned(),
                Some(fields.0.clone()).filter(|f| !f.is_empty()),
            ),
            Self::BadRequest(message) => ("bad_request", message.clone(), None),
            Self::Unauthorized => ("unauthorized", "Authentication required".to_owned(), None),
            Self::InvalidCredentials => ("unauthorized", self.to_string(), None),
            Self::NotFound(_) => ("not_found", self.to_string(), None),
            Self::Conflict { code, message } => (*code, message.clone(), None),
            Self::Internal(_) => ("internal", "Internal server error".to_owned(), None),
        };
        ApiErrorBody {
            code: code.to_owned(),
            message,
            fields,
        }
    }
}

// `IntoResponse` is how axum turns a handler's return value into an HTTP
// response; implementing it for `AppError` lets handlers return `Result<T, AppError>`.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let Self::Internal(err) = &self {
            tracing::error!(error = ?err, "internal error");
        }
        (self.status(), Json(ApiError { error: self.body() })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    async fn render(err: AppError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn validation_is_422_with_fields() {
        let mut fields = FieldErrors::new();
        fields.add("title", "must not be empty");
        let (status, body) = render(AppError::Validation(fields)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "validation_error");
        assert_eq!(body["error"]["fields"]["title"], "must not be empty");
    }

    #[tokio::test]
    async fn empty_fields_are_omitted() {
        let (_, body) = render(AppError::Validation(FieldErrors::new())).await;
        assert!(body["error"].get("fields").is_none());
    }

    #[tokio::test]
    async fn bad_request_is_400() {
        let (status, body) = render(AppError::BadRequest("oops".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "bad_request");
        assert_eq!(body["error"]["message"], "oops");
    }

    #[tokio::test]
    async fn unauthorized_is_401() {
        let (status, body) = render(AppError::Unauthorized).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn invalid_credentials_is_401_with_its_own_message() {
        let (status, body) = render(AppError::InvalidCredentials).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
        assert_eq!(body["error"]["message"], "invalid username or password");
    }

    #[tokio::test]
    async fn not_found_names_the_entity() {
        let (status, body) = render(AppError::NotFound("book")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["message"], "book not found");
    }

    #[tokio::test]
    async fn conflict_uses_the_given_code() {
        let err = AppError::Conflict {
            code: "isbn_taken",
            message: "isbn already used".into(),
        };
        let (status, body) = render(err).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "isbn_taken");
    }

    #[tokio::test]
    async fn internal_does_not_leak_the_inner_message() {
        let (status, body) = render(AppError::Internal(anyhow::anyhow!("secret db detail"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "internal");
        assert!(!body.to_string().contains("secret"));
    }
}
