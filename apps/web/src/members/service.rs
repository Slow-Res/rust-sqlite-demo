//! Member business rules: validation, search normalization, outcome-to-error mapping.
//! Knows nothing about HTTP or SQL.

use std::sync::Arc;

use library_shared::{Member, MemberInput, MemberQuery, Page};

use super::domain::MemberFields;
use super::repo::{DeleteMemberOutcome, MemberFilter, MemberRepository};
use crate::clock::Clock;
use crate::db::RepoError;
use crate::error::AppError;
use crate::pagination::{PageParams, search_term};

/// The unique constraint on `members.email` shows up as this name.
const EMAIL_CONSTRAINT: &str = "members.email";

#[derive(Clone)]
pub struct MemberService {
    repo: Arc<dyn MemberRepository>,
    clock: Arc<dyn Clock>,
}

impl MemberService {
    pub fn new(repo: Arc<dyn MemberRepository>, clock: Arc<dyn Clock>) -> Self {
        Self { repo, clock }
    }

    pub async fn list(&self, query: MemberQuery) -> Result<Page<Member>, AppError> {
        let page = PageParams::new(query.page, query.per_page);
        let filter = MemberFilter {
            q: search_term(query.q)?,
        };
        let (records, total) = self.repo.list(&filter, page).await?;
        Ok(page.into_page(records.into_iter().map(Member::from).collect(), total))
    }

    pub async fn get(&self, id: i64) -> Result<Member, AppError> {
        self.repo
            .get(id)
            .await?
            .map(Member::from)
            .ok_or(AppError::NotFound("member"))
    }

    pub async fn create(&self, input: MemberInput) -> Result<Member, AppError> {
        let fields = MemberFields::parse(input).map_err(AppError::Validation)?;
        let record = self
            .repo
            .create(&fields, self.clock.now())
            .await
            .map_err(map_email_conflict)?;
        Ok(record.into())
    }

    pub async fn update(&self, id: i64, input: MemberInput) -> Result<Member, AppError> {
        let fields = MemberFields::parse(input).map_err(AppError::Validation)?;
        self.repo
            .update(id, &fields, self.clock.now())
            .await
            .map_err(map_email_conflict)?
            .map(Member::from)
            .ok_or(AppError::NotFound("member"))
    }

    pub async fn delete(&self, id: i64) -> Result<(), AppError> {
        match self.repo.delete_checked(id).await? {
            DeleteMemberOutcome::Deleted => Ok(()),
            DeleteMemberOutcome::NotFound => Err(AppError::NotFound("member")),
            DeleteMemberOutcome::HasActiveLoans => Err(AppError::Conflict {
                code: "member_has_active_loans",
                message: "member has active loans and cannot be deleted".to_owned(),
            }),
        }
    }
}

/// A duplicate email is a client-visible 409; any other repo error stays internal.
fn map_email_conflict(err: RepoError) -> AppError {
    match err {
        RepoError::Conflict { constraint } if constraint == EMAIL_CONSTRAINT => {
            AppError::Conflict {
                code: "email_taken",
                message: "a member with this email already exists".to_owned(),
            }
        }
        other => other.into(),
    }
}
