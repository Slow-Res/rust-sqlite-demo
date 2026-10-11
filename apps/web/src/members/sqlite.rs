//! SQLite adapter for [`MemberRepository`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};

use super::domain::MemberFields;
use super::repo::{DeleteMemberOutcome, MemberFilter, MemberRecord, MemberRepository};
use crate::db::{RepoError, escape_like, from_timestamp, to_timestamp};
use crate::pagination::PageParams;

/// `SELECT ... FROM members m`, shaped like [`MemberRow`].
macro_rules! select_members {
    () => {
        concat!(
            "SELECT m.id, m.name, m.email, ",
            "(SELECT COUNT(*) FROM loans l WHERE l.member_id = m.id AND l.returned_at IS NULL) AS active_loans, ",
            "m.created_at, m.updated_at FROM members m"
        )
    };
}

#[derive(Debug, Clone)]
pub struct SqliteMemberRepository {
    pool: SqlitePool,
}

impl SqliteMemberRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// A row as SQLite returns it (raw integer timestamps), before conversion
/// into the adapter-agnostic [`MemberRecord`].
#[derive(sqlx::FromRow)]
struct MemberRow {
    id: i64,
    name: String,
    email: String,
    active_loans: i64,
    created_at: i64,
    updated_at: i64,
}

impl TryFrom<MemberRow> for MemberRecord {
    type Error = RepoError;

    fn try_from(row: MemberRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.id,
            name: row.name,
            email: row.email,
            active_loans: row.active_loans,
            created_at: from_timestamp(row.created_at)?,
            updated_at: from_timestamp(row.updated_at)?,
        })
    }
}

/// Append the `WHERE` clause for `filter`; shared by the page and count queries.
fn push_filter(builder: &mut QueryBuilder<Sqlite>, filter: &MemberFilter) {
    builder.push(" WHERE 1 = 1");
    if let Some(q) = &filter.q {
        let pattern = format!("%{}%", escape_like(q));
        builder.push(" AND (m.name LIKE ");
        builder.push_bind(pattern.clone());
        builder.push(" ESCAPE '\\' OR m.email LIKE ");
        builder.push_bind(pattern);
        builder.push(" ESCAPE '\\')");
    }
}

/// Fetch one member. Takes a connection so it works on the pool and in a transaction.
async fn fetch_member(
    conn: &mut SqliteConnection,
    id: i64,
) -> Result<Option<MemberRecord>, RepoError> {
    let row = sqlx::query_as::<_, MemberRow>(concat!(select_members!(), " WHERE m.id = ?"))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(MemberRecord::try_from).transpose()
}

#[async_trait]
impl MemberRepository for SqliteMemberRepository {
    async fn list(
        &self,
        filter: &MemberFilter,
        page: PageParams,
    ) -> Result<(Vec<MemberRecord>, u64), RepoError> {
        let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM members m");
        push_filter(&mut count, filter);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;

        let mut select = QueryBuilder::<Sqlite>::new(select_members!());
        push_filter(&mut select, filter);
        select.push(" ORDER BY m.name ASC, m.id ASC LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        let rows = select
            .build_query_as::<MemberRow>()
            .fetch_all(&self.pool)
            .await?;

        let items = rows
            .into_iter()
            .map(MemberRecord::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let total = u64::try_from(total).map_err(|err| RepoError::Unexpected(err.into()))?;
        Ok((items, total))
    }

    async fn get(&self, id: i64) -> Result<Option<MemberRecord>, RepoError> {
        let mut conn = self.pool.acquire().await?;
        fetch_member(&mut conn, id).await
    }

    async fn create(
        &self,
        fields: &MemberFields,
        now: DateTime<Utc>,
    ) -> Result<MemberRecord, RepoError> {
        let email = fields.email.as_str();
        let at = to_timestamp(now);
        let result = sqlx::query!(
            "INSERT INTO members (name, email, created_at, updated_at) VALUES (?, ?, ?, ?)",
            fields.name,
            email,
            at,
            at,
        )
        .execute(&self.pool)
        .await?;
        let mut conn = self.pool.acquire().await?;
        fetch_member(&mut conn, result.last_insert_rowid())
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("inserted member vanished")))
    }

    async fn update(
        &self,
        id: i64,
        fields: &MemberFields,
        now: DateTime<Utc>,
    ) -> Result<Option<MemberRecord>, RepoError> {
        let email = fields.email.as_str();
        let at = to_timestamp(now);
        // A single UPDATE is atomic on its own, so no transaction is needed;
        // zero affected rows means the member does not exist.
        let result = sqlx::query!(
            "UPDATE members SET name = ?, email = ?, updated_at = ? WHERE id = ?",
            fields.name,
            email,
            at,
            id,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        let mut conn = self.pool.acquire().await?;
        fetch_member(&mut conn, id).await
    }

    async fn delete_checked(&self, id: i64) -> Result<DeleteMemberOutcome, RepoError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let exists = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM members WHERE id = ?"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if exists == 0 {
            return Ok(DeleteMemberOutcome::NotFound);
        }
        let active = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM loans WHERE member_id = ? AND returned_at IS NULL"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if active > 0 {
            return Ok(DeleteMemberOutcome::HasActiveLoans);
        }

        // Returned loans disappear with the member (ON DELETE CASCADE).
        sqlx::query!("DELETE FROM members WHERE id = ?", id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(DeleteMemberOutcome::Deleted)
    }
}
