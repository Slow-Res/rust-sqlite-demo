//! HTTP layer for `/api/loans`. Every endpoint requires a librarian.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use library_shared::{Loan, LoanQuery, NewLoan, Page};

use super::service::LoanService;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/loans", get(list).post(create))
        .route("/loans/{id}/return", post(return_loan))
}

async fn list(
    State(loans): State<LoanService>,
    _user: AuthUser,
    ApiQuery(query): ApiQuery<LoanQuery>,
) -> Result<ApiJson<Page<Loan>>, AppError> {
    Ok(ApiJson(loans.list(query).await?))
}

/// Borrow a book: `POST /api/loans {"book_id": 1, "member_id": 2}`.
/// `AuthUser` rejects with 401 before the body is even parsed.
async fn create(
    State(loans): State<LoanService>,
    _user: AuthUser,
    ApiJson(input): ApiJson<NewLoan>,
) -> Result<(StatusCode, ApiJson<Loan>), AppError> {
    Ok((StatusCode::CREATED, ApiJson(loans.create(input).await?)))
}

async fn return_loan(
    State(loans): State<LoanService>,
    _user: AuthUser,
    ApiPath(id): ApiPath<i64>,
) -> Result<ApiJson<Loan>, AppError> {
    Ok(ApiJson(loans.return_loan(id).await?))
}
