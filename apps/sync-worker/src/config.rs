//! Settings from environment variables (spec §9).

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{Context, bail};
use sqlx::postgres::PgConnectOptions;

/// Largest accepted BATCH_SIZE.
const MAX_BATCH_SIZE: usize = 10_000;

/// Output format of the tracing subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Clone)]
pub struct Config {
    /// This node's name; also its `node_id` in Postgres.
    pub node_id: String,
    /// The node's SQLite file, e.g. `sqlite:///data/library.db`.
    pub database_url: String,
    /// The central Postgres. Never printed: it holds a password.
    pub postgres: PgConnectOptions,
    /// Most change_log rows read per batch.
    pub batch_size: usize,
    /// Address of the /health and /metrics server.
    pub metrics_addr: SocketAddr,
    pub log_format: LogFormat,
}

// Written by hand so the Postgres password can never reach a log line.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let postgres = format!(
            "{}@{}:{}/{}",
            self.postgres.get_username(),
            self.postgres.get_host(),
            self.postgres.get_port(),
            self.postgres.get_database().unwrap_or("")
        );
        f.debug_struct("Config")
            .field("node_id", &self.node_id)
            .field("database_url", &self.database_url)
            .field("postgres", &postgres)
            .field("batch_size", &self.batch_size)
            .field("metrics_addr", &self.metrics_addr)
            .field("log_format", &self.log_format)
            .finish()
    }
}

impl Config {
    /// Read the process environment. Fails fast with a message naming the bad variable.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build a config from any key lookup, so tests need not touch the real environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let node_id = get("NODE_ID").context("NODE_ID is required")?;
        if !library_shared::is_valid_node_id(&node_id) {
            bail!(
                "NODE_ID must be 1-40 characters of a-z, 0-9 and `-`, starting with a letter or digit; got `{node_id}`"
            );
        }

        let postgres_url = get("POSTGRES_URL").context("POSTGRES_URL is required")?;
        // The URL holds the password, so this error must not repeat it.
        let postgres = PgConnectOptions::from_str(&postgres_url)
            .map_err(|_| anyhow::anyhow!("POSTGRES_URL is not a valid Postgres URL"))?
            .application_name(&format!("sync-worker {node_id}"));

        let batch_size: usize = parse_or(&get, "BATCH_SIZE", 500)?;
        if !(1..=MAX_BATCH_SIZE).contains(&batch_size) {
            bail!("BATCH_SIZE must be between 1 and {MAX_BATCH_SIZE}, got {batch_size}");
        }

        let log_format = match get("LOG_FORMAT").as_deref() {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(other) => bail!("LOG_FORMAT must be `pretty` or `json`, got `{other}`"),
        };

        Ok(Self {
            node_id,
            database_url: get("DATABASE_URL")
                .unwrap_or_else(|| "sqlite://data/library.db".to_owned()),
            postgres,
            batch_size,
            metrics_addr: parse_or(
                &get,
                "METRICS_ADDR",
                SocketAddr::from(([127, 0, 0, 1], 9100)),
            )?,
            log_format,
        })
    }
}

/// Parse `key` if present, otherwise use `default`.
fn parse_or<T>(get: &impl Fn(&str) -> Option<String>, key: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
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

    const PG: &str = "postgres://sync:s3cret-pw@db.example:5432/library";

    fn lookup<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn minimal_settings_and_defaults() {
        let config =
            Config::from_lookup(lookup(&[("NODE_ID", "node1"), ("POSTGRES_URL", PG)])).unwrap();
        assert_eq!(config.node_id, "node1");
        assert_eq!(config.database_url, "sqlite://data/library.db");
        assert_eq!(config.batch_size, 500);
        assert_eq!(config.metrics_addr.to_string(), "127.0.0.1:9100");
        assert_eq!(config.log_format, LogFormat::Pretty);
        assert_eq!(config.postgres.get_host(), "db.example");
    }

    #[test]
    fn node_id_and_postgres_url_are_required() {
        let err = Config::from_lookup(lookup(&[("POSTGRES_URL", PG)])).unwrap_err();
        assert!(err.to_string().contains("NODE_ID"), "{err}");
        let err = Config::from_lookup(lookup(&[("NODE_ID", "node1")])).unwrap_err();
        assert!(err.to_string().contains("POSTGRES_URL"), "{err}");
    }

    #[test]
    fn bad_node_ids_are_refused() {
        let long = "a".repeat(41);
        for bad in [
            "",
            "Node1",
            "node_1",
            "-node",
            "a/b",
            "../x",
            "node1\nx",
            long.as_str(),
        ] {
            let err =
                Config::from_lookup(lookup(&[("NODE_ID", bad), ("POSTGRES_URL", PG)])).unwrap_err();
            assert!(err.to_string().contains("NODE_ID"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn the_postgres_password_never_appears_in_errors_or_debug_output() {
        let err = Config::from_lookup(lookup(&[
            ("NODE_ID", "node1"),
            ("POSTGRES_URL", "postgres://u:s3cret-pw@host:notaport/db"),
        ]))
        .unwrap_err();
        assert!(!format!("{err:#}").contains("s3cret-pw"), "{err:#}");
        let config =
            Config::from_lookup(lookup(&[("NODE_ID", "node1"), ("POSTGRES_URL", PG)])).unwrap();
        assert!(!format!("{config:?}").contains("s3cret-pw"));
    }

    #[test]
    fn batch_size_must_be_in_range() {
        for bad in ["0", "10001", "many"] {
            let err = Config::from_lookup(lookup(&[
                ("NODE_ID", "node1"),
                ("POSTGRES_URL", PG),
                ("BATCH_SIZE", bad),
            ]))
            .unwrap_err();
            assert!(err.to_string().contains("BATCH_SIZE"), "{bad}: {err}");
        }
    }
}
