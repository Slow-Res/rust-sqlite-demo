//! Authentication: librarian accounts, password hashing, and cookie sessions (spec §5.7).
//!
//! Layers: `routes` (HTTP) -> `service` (rules) -> `repo` (port) <- `sqlite` (adapter).
//! `extractor` is the one piece other features use: take [`AuthUser`] as a
//! handler argument to require a logged-in librarian.

mod extractor;
mod password;
#[cfg(feature = "postgres")]
mod postgres;
mod repo;
mod routes;
mod service;
mod sqlite;

pub use extractor::AuthUser;
#[cfg(feature = "postgres")]
pub use postgres::PostgresAuthRepository;
pub use repo::{AuthRepository, SessionUser, UserRecord};
pub use routes::routes;
pub use service::{AuthService, LoginSession};
pub use sqlite::SqliteAuthRepository;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "library_session";
