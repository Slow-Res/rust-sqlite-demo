//! HTTP layer for `/api/books`. Reads are public; writes require a librarian.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use library_shared::{Book, BookInput, BookQuery, Page};

use super::service::BookService;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/books", get(list).post(create))
        .route("/books/{id}", get(show).put(update).delete(remove))
}

async fn list(
    State(books): State<BookService>,
    ApiQuery(query): ApiQuery<BookQuery>,
) -> Result<ApiJson<Page<Book>>, AppError> {
    Ok(ApiJson(books.list(query).await?))
}

async fn show(
    State(books): State<BookService>,
    ApiPath(id): ApiPath<i64>,
) -> Result<ApiJson<Book>, AppError> {
    Ok(ApiJson(books.get(id).await?))
}

// Taking `_user: AuthUser` is what protects a route: the extractor rejects the
// request with 401 before the handler body runs. The value itself is unused.
async fn create(
    State(books): State<BookService>,
    _user: AuthUser,
    ApiJson(input): ApiJson<BookInput>,
) -> Result<(StatusCode, ApiJson<Book>), AppError> {
    Ok((StatusCode::CREATED, ApiJson(books.create(input).await?)))
}

async fn update(
    State(books): State<BookService>,
    _user: AuthUser,
    ApiPath(id): ApiPath<i64>,
    ApiJson(input): ApiJson<BookInput>,
) -> Result<ApiJson<Book>, AppError> {
    Ok(ApiJson(books.update(id, input).await?))
}

async fn remove(
    State(books): State<BookService>,
    _user: AuthUser,
    ApiPath(id): ApiPath<i64>,
) -> Result<StatusCode, AppError> {
    books.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
