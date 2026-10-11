//! Shared application state handed to every handler.

use std::sync::Arc;

use crate::auth::{AuthRepository, AuthService, SqliteAuthRepository};
use crate::books::{BookRepository, BookService, SqliteBookRepository};
use crate::clock::Clock;
use crate::config::Config;
use crate::db::Database;
use crate::loans::{LoanRepository, LoanService, SqliteLoanRepository};
use crate::members::{MemberRepository, MemberService, SqliteMemberRepository};
#[cfg(feature = "postgres")]
use crate::{
    auth::PostgresAuthRepository, books::PostgresBookRepository, loans::PostgresLoanRepository,
    members::PostgresMemberRepository,
};
use axum::extract::FromRef;

/// Everything handlers need. Feature services (auth, books, ...) are added as
/// new public fields and built inside [`AppState::new`].
///
/// `#[derive(FromRef)]` generates a `FromRef<AppState>` impl for each field,
/// so a handler can ask for just the piece it needs, e.g.
/// `State(db): State<Database>`, instead of the whole state.
#[derive(Clone, FromRef)]
pub struct AppState {
    pub db: Database,
    pub config: Arc<Config>,
    /// `Arc<dyn Clock>`: a cheap-to-clone handle to *some* clock, chosen at startup
    /// (the real one in production, a fixed one in tests).
    pub clock: Arc<dyn Clock>,
    pub auth: AuthService,
    pub books: BookService,
    pub members: MemberService,
    pub loans: LoanService,
}

/// The auth adapter for the backend `db` is. `create-user` needs only this one.
pub fn auth_repository(db: &Database) -> Arc<dyn AuthRepository> {
    match db {
        Database::Sqlite(pool) => Arc::new(SqliteAuthRepository::new(pool.clone())),
        #[cfg(feature = "postgres")]
        Database::Postgres(pool) => Arc::new(PostgresAuthRepository::new(pool.clone())),
    }
}

impl AppState {
    pub fn new(db: impl Into<Database>, config: Arc<Config>, clock: Arc<dyn Clock>) -> Self {
        let db = db.into();
        let auth = AuthService::new(
            auth_repository(&db),
            clock.clone(),
            chrono::Duration::hours(config.session_ttl_hours),
        );
        let (book_repo, member_repo, loan_repo): (
            Arc<dyn BookRepository>,
            Arc<dyn MemberRepository>,
            Arc<dyn LoanRepository>,
        ) = match &db {
            Database::Sqlite(pool) => (
                Arc::new(SqliteBookRepository::new(pool.clone())),
                Arc::new(SqliteMemberRepository::new(pool.clone())),
                Arc::new(SqliteLoanRepository::new(pool.clone())),
            ),
            #[cfg(feature = "postgres")]
            Database::Postgres(pool) => (
                Arc::new(PostgresBookRepository::new(pool.clone())),
                Arc::new(PostgresMemberRepository::new(pool.clone())),
                Arc::new(PostgresLoanRepository::new(pool.clone())),
            ),
        };
        let books = BookService::new(book_repo, clock.clone());
        let members = MemberService::new(member_repo, clock.clone());
        let loans = LoanService::new(loan_repo, clock.clone());
        Self {
            db,
            config,
            clock,
            auth,
            books,
            members,
            loans,
        }
    }
}
