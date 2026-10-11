//! HTTP layer for `/api/members`. Every endpoint requires a librarian.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use library_shared::{Member, MemberInput, MemberQuery, Page};

use super::service::MemberService;
use crate::auth::AuthUser;
use crate::error::AppError;
use crate::extract::{ApiJson, ApiPath, ApiQuery};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/members", get(list).post(create))
        .route("/members/{id}", get(show).put(update).delete(remove))
}

async fn list(
    State(members): State<MemberService>,
    _user: AuthUser,
    ApiQuery(query): ApiQuery<MemberQuery>,
) -> Result<ApiJson<Page<Member>>, AppError> {
    Ok(ApiJson(members.list(query).await?))
}

async fn show(
    State(members): State<MemberService>,
    _user: AuthUser,
    ApiPath(id): ApiPath<i64>,
) -> Result<ApiJson<Member>, AppError> {
    Ok(ApiJson(members.get(id).await?))
}

async fn create(
    State(members): State<MemberService>,
    _user: AuthUser,
    ApiJson(input): ApiJson<MemberInput>,
) -> Result<(StatusCode, ApiJson<Member>), AppError> {
    Ok((StatusCode::CREATED, ApiJson(members.create(input).await?)))
}

async fn update(
    State(members): State<MemberService>,
    _user: AuthUser,
    ApiPath(id): ApiPath<i64>,
    ApiJson(input): ApiJson<MemberInput>,
) -> Result<ApiJson<Member>, AppError> {
    Ok(ApiJson(members.update(id, input).await?))
}

async fn remove(
    State(members): State<MemberService>,
    _user: AuthUser,
    ApiPath(id): ApiPath<i64>,
) -> Result<StatusCode, AppError> {
    members.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
