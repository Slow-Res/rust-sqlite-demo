//! Shared application state handed to every handler.

use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::SqlitePool;

use crate::auth::{AuthService, SqliteAuthRepository};
use crate::books::{BookService, SqliteBookRepository};
use crate::clock::Clock;
use crate::config::Config;
use crate::loans::{LoanService, SqliteLoanRepository};
use crate::members::{MemberService, SqliteMemberRepository};

/// Everything handlers need. Feature services (auth, books, ...) are added as
/// new public fields and built inside [`AppState::new`].
///
/// `#[derive(FromRef)]` generates a `FromRef<AppState>` impl for each field,
/// so a handler can ask for just the piece it needs, e.g.
/// `State(pool): State<SqlitePool>`, instead of the whole state.
#[derive(Clone, FromRef)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    /// `Arc<dyn Clock>`: a cheap-to-clone handle to *some* clock, chosen at startup
    /// (the real one in production, a fixed one in tests).
    pub clock: Arc<dyn Clock>,
    pub auth: AuthService,
    pub books: BookService,
    pub members: MemberService,
    pub loans: LoanService,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Arc<Config>, clock: Arc<dyn Clock>) -> Self {
        let auth = AuthService::new(
            Arc::new(SqliteAuthRepository::new(pool.clone())),
            clock.clone(),
            chrono::Duration::hours(config.session_ttl_hours),
        );
        let books = BookService::new(
            Arc::new(SqliteBookRepository::new(pool.clone())),
            clock.clone(),
        );
        let members = MemberService::new(
            Arc::new(SqliteMemberRepository::new(pool.clone())),
            clock.clone(),
        );
        let loans = LoanService::new(
            Arc::new(SqliteLoanRepository::new(pool.clone())),
            clock.clone(),
        );
        Self {
            pool,
            config,
            clock,
            auth,
            books,
            members,
            loans,
        }
    }
}
