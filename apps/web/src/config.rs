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

        Ok(Self {
            database_url: get("DATABASE_URL")
                .unwrap_or_else(|| "sqlite://data/library.db".to_owned()),
            bind_addr: parse_or(&get, "BIND_ADDR", SocketAddr::from(([127, 0, 0, 1], 4000)))?,
            frontend_dir: get("FRONTEND_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            cookie_secure: parse_or(&get, "COOKIE_SECURE", false)?,
            session_ttl_hours,
            log_format,
        })
    }
}

/// Parse `key` if present, otherwise use `default`.
fn parse_or<T>(get: &impl Fn(&str) -> Option<String>, key: &str, default: T) -> anyhow::Result<T>
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
