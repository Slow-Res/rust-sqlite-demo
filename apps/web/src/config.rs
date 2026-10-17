//! Runtime configuration read from environment variables (spec §5.3).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};

/// Ten years.
const MAX_SESSION_TTL_HOURS: i64 = 87_600;

/// Output format of the tracing subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    /// When set, this directory is served as the SPA frontend.
    pub frontend_dir: Option<PathBuf>,
    pub cookie_secure: bool,
    pub session_ttl_hours: i64,
    pub log_format: LogFormat,
    /// This node's name (spec §9); shown by /api/health.
    pub node_id: String,
    /// How long the total of an unfiltered list page may be reused, in ms; 0
    /// counts on every request (see `count_cache`).
    pub list_count_ttl_ms: u64,
    /// SQLite only: how often, in ms, to checkpoint and truncate the WAL in the
    /// background (see `checkpoint`); 0 leaves it to SQLite (and Litestream).
    pub sqlite_checkpoint_interval_ms: u64,
    /// How many password hashes may run at once; `None` takes half the cores (see
    /// `auth::password`).
    pub password_hash_permits: Option<usize>,
    /// Size of the database pool, on either backend; `None` takes the default of
    /// `db` (10).
    pub database_max_connections: Option<u32>,
    /// Background jobs (Postgres only; see `jobs_config`).
    pub jobs: crate::jobs_config::JobsConfig,
}

impl Config {
    /// Read the process environment. Fails fast with a message naming the bad variable.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build a config from any key lookup. Taking a closure (instead of reading
    /// `std::env` directly) lets unit tests supply values without touching
    /// process-global state.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let session_ttl_hours: i64 = parse_or(&get, "SESSION_TTL_HOURS", 168)?;
        // The upper bound keeps `Duration::hours` and `now + ttl` from overflowing.
        if !(1..=MAX_SESSION_TTL_HOURS).contains(&session_ttl_hours) {
            bail!(
                "SESSION_TTL_HOURS must be between 1 and {MAX_SESSION_TTL_HOURS}, got {session_ttl_hours}"
            );
        }

        let log_format = match get("LOG_FORMAT").as_deref() {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(other) => bail!("LOG_FORMAT must be `pretty` or `json`, got `{other}`"),
        };

        let node_id = get("NODE_ID").unwrap_or_else(|| "local".to_owned());
        if !library_shared::is_valid_node_id(&node_id) {
            bail!(
                "NODE_ID must be 1-40 characters of a-z, 0-9 and `-`, starting with a letter or digit; got `{node_id}`"
            );
        }

        let password_hash_permits = match get("PASSWORD_HASH_PERMITS") {
            None => None,
            Some(raw) => match raw.parse::<usize>() {
                Ok(n) if n > 0 => Some(n),
                _ => bail!("PASSWORD_HASH_PERMITS must be a positive integer, got `{raw}`"),
            },
        };

        let database_max_connections = match get("DATABASE_MAX_CONNECTIONS") {
            None => None,
            Some(raw) => match raw.parse::<u32>() {
                Ok(n) if n > 0 => Some(n),
                _ => bail!("DATABASE_MAX_CONNECTIONS must be a positive integer, got `{raw}`"),
            },
        };

        let database_url =
            get("DATABASE_URL").unwrap_or_else(|| "sqlite://data/library.db".to_owned());
        let jobs = crate::jobs_config::JobsConfig::from_lookup(&get)?;
        if jobs.enabled && !crate::db::is_postgres_url(&database_url) {
            bail!(
                "JOBS_ENABLED=true needs a Postgres DATABASE_URL: background jobs are Postgres only"
            );
        }

        Ok(Self {
            database_url,
            bind_addr: parse_or(&get, "BIND_ADDR", SocketAddr::from(([127, 0, 0, 1], 4000)))?,
            frontend_dir: get("FRONTEND_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            cookie_secure: parse_or(&get, "COOKIE_SECURE", false)?,
            session_ttl_hours,
            log_format,
            node_id,
            list_count_ttl_ms: parse_or(&get, "LIST_COUNT_TTL_MS", 1000)?,
            sqlite_checkpoint_interval_ms: parse_or(&get, "SQLITE_CHECKPOINT_INTERVAL_MS", 0)?,
            password_hash_permits,
            database_max_connections,
            jobs,
        })
    }
}

/// Parse `key` if present, otherwise use `default`.
pub(crate) fn parse_or<T>(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: T,
) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match get(key) {
        None => Ok(default),
        Some(raw) => raw
            .parse()
            .with_context(|| format!("invalid value for {key}: `{raw}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn defaults_apply_when_unset() {
        let config = Config::from_lookup(lookup(&[])).unwrap();
        assert_eq!(config.database_url, "sqlite://data/library.db");
        assert_eq!(config.bind_addr.to_string(), "127.0.0.1:4000");
        assert!(config.frontend_dir.is_none());
        assert!(!config.cookie_secure);
        assert_eq!(config.session_ttl_hours, 168);
        assert_eq!(config.log_format, LogFormat::Pretty);
        assert_eq!(config.list_count_ttl_ms, 1000);
        assert_eq!(config.sqlite_checkpoint_interval_ms, 0);
        assert_eq!(config.password_hash_permits, None);
        assert_eq!(config.database_max_connections, None);
    }

    #[test]
    fn jobs_need_a_postgres_database() {
        let err = Config::from_lookup(lookup(&[("JOBS_ENABLED", "true")])).unwrap_err();
        assert!(err.to_string().contains("JOBS_ENABLED"), "{err}");
        let ok = Config::from_lookup(lookup(&[
            ("JOBS_ENABLED", "true"),
            ("DATABASE_URL", "postgres://u:p@localhost/db"), // local only
        ]))
        .unwrap();
        assert!(ok.jobs.enabled);
    }

    #[test]
    fn database_max_connections_must_be_a_positive_integer() {
        let pool = |v: &str| Config::from_lookup(lookup(&[("DATABASE_MAX_CONNECTIONS", v)]));
        assert_eq!(pool("20").unwrap().database_max_connections, Some(20));
        for bad in ["0", "-1", "", "ten", "1.5", "4294967296"] {
            let err = pool(bad).unwrap_err();
            assert!(
                err.to_string().contains("DATABASE_MAX_CONNECTIONS"),
                "{bad}"
            );
        }
    }

    #[test]
    fn password_hash_permits_must_be_a_positive_integer() {
        let permits = |v: &str| Config::from_lookup(lookup(&[("PASSWORD_HASH_PERMITS", v)]));
        assert_eq!(permits("3").unwrap().password_hash_permits, Some(3));
        for bad in ["0", "-1", "", "two", "1.5"] {
            let err = permits(bad).unwrap_err();
            assert!(err.to_string().contains("PASSWORD_HASH_PERMITS"), "{bad}");
        }
    }

    #[test]
    fn checkpoint_interval_is_configurable() {
        let interval =
            |v: &str| Config::from_lookup(lookup(&[("SQLITE_CHECKPOINT_INTERVAL_MS", v)]));
        assert_eq!(
            interval("1000").unwrap().sqlite_checkpoint_interval_ms,
            1000
        );
        let err = interval("-1").unwrap_err();
        assert!(err.to_string().contains("SQLITE_CHECKPOINT_INTERVAL_MS"));
    }

    #[test]
    fn list_count_ttl_is_configurable_and_zero_is_allowed() {
        let ttl = |v: &str| Config::from_lookup(lookup(&[("LIST_COUNT_TTL_MS", v)]));
        assert_eq!(ttl("0").unwrap().list_count_ttl_ms, 0);
        assert_eq!(ttl("250").unwrap().list_count_ttl_ms, 250);
        let err = ttl("-1").unwrap_err();
        assert!(err.to_string().contains("LIST_COUNT_TTL_MS"));
    }

    #[test]
    fn node_id_defaults_to_local() {
        let config = Config::from_lookup(lookup(&[])).unwrap();
        assert_eq!(config.node_id, "local");
    }

    #[test]
    fn bad_node_id_is_refused_with_its_name() {
        for bad in ["", "Node1", "a/b", "node1\nx"] {
            let err = Config::from_lookup(lookup(&[("NODE_ID", bad)])).unwrap_err();
            assert!(err.to_string().contains("NODE_ID"), "{err}");
        }
    }

    #[test]
    fn overrides_are_parsed() {
        let config = Config::from_lookup(lookup(&[
            ("BIND_ADDR", "0.0.0.0:8080"),
            ("COOKIE_SECURE", "true"),
            ("LOG_FORMAT", "json"),
            ("FRONTEND_DIR", "dist"),
        ]))
        .unwrap();
        assert_eq!(config.bind_addr.port(), 8080);
        assert!(config.cookie_secure);
        assert_eq!(config.log_format, LogFormat::Json);
        assert_eq!(config.frontend_dir, Some(PathBuf::from("dist")));
    }

    #[test]
    fn invalid_values_fail_with_the_variable_name() {
        let err = Config::from_lookup(lookup(&[("BIND_ADDR", "nope")])).unwrap_err();
        assert!(err.to_string().contains("BIND_ADDR"));
        assert!(Config::from_lookup(lookup(&[("LOG_FORMAT", "xml")])).is_err());
        assert!(Config::from_lookup(lookup(&[("SESSION_TTL_HOURS", "0")])).is_err());
    }

    #[test]
    fn session_ttl_is_bounded() {
        let ttl = |v: &str| Config::from_lookup(lookup(&[("SESSION_TTL_HOURS", v)]));
        assert_eq!(ttl("1").unwrap().session_ttl_hours, 1);
        assert_eq!(ttl("87600").unwrap().session_ttl_hours, 87_600);
        for bad in ["0", "-5", "87601", "9223372036854775807"] {
            let err = ttl(bad).unwrap_err();
            assert!(err.to_string().contains("SESSION_TTL_HOURS"), "{bad}");
        }
    }
}
