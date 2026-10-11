//! Shared helpers for integration tests.
//!
//! Each file in `tests/` is compiled as its own crate and uses only some of
//! these helpers, hence the module-level `dead_code` allowance. Panicking on a
//! broken test setup is the right behavior here, but clippy's "allow in tests"
//! setting only covers `#[test]` functions, so allow it explicitly.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use library_web::auth::AuthService;
use library_web::build_router;
use library_web::clock::FixedClock;
use library_web::config::{Config, LogFormat};
use library_web::db::Database;
use library_web::state::AppState;
use serde_json::Value;
use tower::ServiceExt;

/// Turns one test body into a SQLite test and, with the `postgres` feature, a
/// Postgres test (`<name>::sqlite` and `<name>::postgres`). Each body is written
/// once as `async fn name(db: Database) { ... }` (`Database` is only a token there, not imported) and builds its app with
/// `TestApp::new(db)`. Postgres tests get a fresh database each, dropped at the end.
#[macro_export]
macro_rules! db_tests {
    ($( async fn $name:ident($db:ident: Database) $body:block )*) => {$(
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[allow(clippy::unwrap_used, clippy::expect_used)]
            async fn body($db: library_web::db::Database) $body

            #[sqlx::test(migrations = "../../migrations/sqlite")]
            async fn sqlite(pool: sqlx::SqlitePool) {
                body(pool.into()).await;
            }

            // Multi-threaded so the concurrency tests really run in parallel.
            #[cfg(feature = "postgres")]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn postgres() {
                $crate::common::on_postgres(body).await;
            }
        }
    )*};
}

/// Status, headers and parsed JSON body (`Value::Null` when the body is empty).
pub type TestResponse = (StatusCode, HeaderMap, Value);

/// The real router wired to a fresh test database and a controllable clock.
/// Cloning is cheap (everything inside is a handle), which lets tests fire concurrent requests.
#[derive(Clone)]
pub struct TestApp {
    pub router: Router,
    pub auth: AuthService,
    pub clock: Arc<FixedClock>,
    pub db: Database,
}

/// Start of the fake clock: 2027-01-15 08:00:00 UTC.
pub fn start_time() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).expect("valid constant timestamp")
}

impl TestApp {
    /// Build the app from the database a test gets (already migrated, unique per
    /// test): the `Database` from `db_tests!`, or the pool `#[sqlx::test(...)]` hands over.
    pub fn new(db: impl Into<Database>) -> Self {
        Self::with_config(db.into(), test_config())
    }

    /// Like [`new`](Self::new) but serving the SPA from `dir`.
    pub fn with_frontend_dir(db: impl Into<Database>, dir: PathBuf) -> Self {
        Self::with_config(
            db.into(),
            Config {
                frontend_dir: Some(dir),
                ..test_config()
            },
        )
    }

    /// Like [`new`](Self::new) but with `COOKIE_SECURE=true`.
    pub fn with_secure_cookie(db: impl Into<Database>) -> Self {
        Self::with_config(
            db.into(),
            Config {
                cookie_secure: true,
                ..test_config()
            },
        )
    }

    fn with_config(db: Database, config: Config) -> Self {
        let clock = Arc::new(FixedClock::new(start_time()));
        let config = Arc::new(config);
        let state = AppState::new(db.clone(), config, clock.clone());
        Self {
            auth: state.auth.clone(),
            router: build_router(state),
            clock,
            db,
        }
    }

    /// Run `sql` (written with `$1`, `$2`... placeholders, which both SQLite and
    /// Postgres accept) and return the first column of its one row as an integer.
    /// For counts, and for `INSERT ... RETURNING id`.
    pub async fn scalar(&self, sql: &'static str, args: &[Arg]) -> i64 {
        match &self.db {
            Database::Sqlite(pool) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for arg in args {
                    query = match arg {
                        Arg::Int(n) => query.bind(*n),
                        Arg::OptInt(n) => query.bind(*n),
                        Arg::Text(text) => query.bind(text.clone()),
                        Arg::OptText(text) => query.bind(text.clone()),
                    };
                }
                query.fetch_one(pool).await.expect("scalar query")
            }
            #[cfg(feature = "postgres")]
            Database::Postgres(pool) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for arg in args {
                    query = match arg {
                        Arg::Int(n) => query.bind(*n),
                        Arg::OptInt(n) => query.bind(*n),
                        Arg::Text(text) => query.bind(text.clone()),
                        Arg::OptText(text) => query.bind(text.clone()),
                    };
                }
                query.fetch_one(pool).await.expect("scalar query")
            }
        }
    }

    /// Like [`scalar`](Self::scalar) for a first column of text.
    pub async fn text(&self, sql: &'static str) -> String {
        match &self.db {
            Database::Sqlite(pool) => sqlx::query_scalar(sql).fetch_one(pool).await,
            #[cfg(feature = "postgres")]
            Database::Postgres(pool) => sqlx::query_scalar(sql).fetch_one(pool).await,
        }
        .expect("text query")
    }

    pub async fn get(&self, uri: &str, cookie: Option<&str>) -> TestResponse {
        self.send(Method::GET, uri, None, cookie).await
    }

    pub async fn post_json(&self, uri: &str, body: &Value, cookie: Option<&str>) -> TestResponse {
        self.send(Method::POST, uri, Some(body), cookie).await
    }

    pub async fn put_json(&self, uri: &str, body: &Value, cookie: Option<&str>) -> TestResponse {
        self.send(Method::PUT, uri, Some(body), cookie).await
    }

    pub async fn delete(&self, uri: &str, cookie: Option<&str>) -> TestResponse {
        self.send(Method::DELETE, uri, None, cookie).await
    }

    /// Create a librarian through the auth service, log in over HTTP, and
    /// return the `Cookie` header value (`library_session=...`) to pass to
    /// the request helpers.
    pub async fn login(&self, username: &str, password: &str) -> String {
        self.auth
            .create_user(username, password)
            .await
            .expect("create test user");
        let body = serde_json::json!({ "username": username, "password": password });
        let (status, headers, _) = self.post_json("/api/auth/login", &body, None).await;
        assert_eq!(status, StatusCode::OK, "test login failed");
        let set_cookie = headers
            .get(header::SET_COOKIE)
            .expect("login sets a cookie")
            .to_str()
            .expect("cookie header is text");
        set_cookie
            .split(';')
            .next()
            .expect("cookie has a name=value pair")
            .to_owned()
    }

    /// Send a request and return status, headers and the body as text, for
    /// responses that are not JSON (static files, middleware rejections).
    pub async fn send_text(
        &self,
        method: Method,
        uri: &str,
        body: Body,
    ) -> (StatusCode, HeaderMap, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .expect("valid test request");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router is infallible");
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.expect("readable body").to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    /// Send a request through the router without opening a socket.
    /// `cookie` is a raw `Cookie` header value such as `library_session=...`.
    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Option<&Value>,
        cookie: Option<&str>,
    ) -> TestResponse {
        self.send_raw(method, uri, body.map(Value::to_string), cookie)
            .await
    }

    /// Like [`send`](Self::send) but with an arbitrary JSON-typed body, so
    /// tests can post malformed JSON.
    pub async fn send_raw(
        &self,
        method: Method,
        uri: &str,
        body: Option<String>,
        cookie: Option<&str>,
    ) -> TestResponse {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let request = match body {
            Some(text) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(text)),
            None => builder.body(Body::empty()),
        }
        .expect("valid test request");

        // `oneshot` consumes the service, so clone the (cheap) router per call.
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router is infallible");
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.expect("readable body").to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("response body is JSON")
        };
        (parts.status, parts.headers, json)
    }
}

/// A bind value for [`TestApp::scalar`].
pub enum Arg {
    Int(i64),
    OptInt(Option<i64>),
    Text(String),
    OptText(Option<String>),
}

impl From<i64> for Arg {
    fn from(n: i64) -> Self {
        Self::Int(n)
    }
}

impl From<Option<i64>> for Arg {
    fn from(n: Option<i64>) -> Self {
        Self::OptInt(n)
    }
}

impl From<&str> for Arg {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<Option<String>> for Arg {
    fn from(text: Option<String>) -> Self {
        Self::OptText(text)
    }
}

impl From<String> for Arg {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

pub fn test_config() -> Config {
    Config {
        // Unused: the pool comes from `#[sqlx::test(...)]`, not from this URL.
        database_url: "sqlite::memory:".to_owned(),
        bind_addr: ([127, 0, 0, 1], 0).into(),
        frontend_dir: None,
        cookie_secure: false,
        session_ttl_hours: 168,
        log_format: LogFormat::Pretty,
        node_id: "test-node".to_owned(),
    }
}

/// Insert a fresh book and a loan of it for `member_id` straight into the
/// database, bypassing the API (handy for setting up loan counts).
pub async fn insert_loan(app: &TestApp, member_id: i64, returned: bool) {
    let n = app.scalar("SELECT COUNT(*) FROM books", &[]).await;
    let book_id = app
        .scalar(
            "INSERT INTO books (title, author, total_copies, created_at, updated_at) VALUES ($1, 'A', 5, 0, 0) RETURNING id",
            &[format!("Book {n}").into()],
        )
        .await;
    app.scalar(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES ($1, $2, 0, 100, $3) RETURNING id",
        &[book_id.into(), member_id.into(), returned.then_some(50).into()],
    )
    .await;
}

/// A fresh, empty database on the test Postgres, one per test. The server comes
/// from TEST_POSTGRES_URL; `make test` starts one in Docker. Same pattern as
/// `TestPg` in the sync worker's tests.
#[cfg(feature = "postgres")]
struct TestPg {
    admin: sqlx::postgres::PgConnectOptions,
    name: String,
    options: sqlx::postgres::PgConnectOptions,
}

#[cfg(feature = "postgres")]
impl TestPg {
    async fn new() -> Self {
        use sqlx::Connection;
        let url = std::env::var("TEST_POSTGRES_URL").expect(
            "TEST_POSTGRES_URL is not set. Run `make test` (it starts a throwaway Postgres in Docker), \
             or set it to any Postgres where you may create databases.",
        );
        let admin = std::str::FromStr::from_str(&url).expect("valid TEST_POSTGRES_URL");
        let name = unique_name();
        let mut conn = sqlx::PgConnection::connect_with(&admin)
            .await
            .expect("connect to test Postgres");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.ok();
        let options = sqlx::postgres::PgConnectOptions::clone(&admin).database(&name);
        Self {
            admin,
            name,
            options,
        }
    }

    async fn cleanup(self) {
        use sqlx::Connection;
        let mut conn = sqlx::PgConnection::connect_with(&self.admin)
            .await
            .expect("connect to test Postgres");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut conn)
        .await
        .expect("drop test database");
        conn.close().await.ok();
    }
}

#[cfg(feature = "postgres")]
fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "web_test_{}_{nanos}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Run a test body against a fresh, migrated Postgres database, and drop the
/// database afterwards even when the body panics (it runs in its own task, so
/// the panic is caught, the database dropped, and the panic re-raised).
#[cfg(feature = "postgres")]
pub async fn on_postgres<F, Fut>(body: F)
where
    F: FnOnce(Database) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let pg = TestPg::new().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect_with(pg.options.clone())
        .await
        .expect("connect to test database");
    let db = Database::Postgres(pool.clone());
    library_web::db::migrate(&db).await.expect("migrate");
    let outcome = tokio::spawn(body(db)).await;
    pool.close().await;
    pg.cleanup().await;
    if let Err(err) = outcome {
        match err.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(err) => panic!("test task failed: {err}"),
        }
    }
}
