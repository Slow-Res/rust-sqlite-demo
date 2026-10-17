//! Background-job settings (docs/JOBS.md, spec §18). Plain data, compiled into
//! every build so `Config` has one shape; only the `postgres` feature acts on it.

use std::fmt;
use std::net::SocketAddr;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::config::parse_or;

/// A value that must never reach a log: `Debug` prints `***`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// An outgoing-webhook receiver. Only operators configure these, never API input
/// (spec §15.3: no SSRF surface).
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookEndpoint {
    pub url: String,
    /// `whsec_<base64 key>`, the Standard Webhooks format.
    pub secret: Secret,
}

/// Receivers often carry a token in the path, query or userinfo, so `Debug`
/// shows only `scheme://host`.
impl fmt::Debug for WebhookEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookEndpoint")
            .field("url", &redacted_url(&self.url))
            .field("secret", &self.secret)
            .finish()
    }
}

pub(crate) fn redacted_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "***".to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.is_empty() {
        "***".to_owned()
    } else {
        format!("{scheme}://{host}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobsConfig {
    /// `JOBS_ENABLED`: enqueue on borrow/return, migrate the jobs schema, allow `worker`.
    pub enabled: bool,
    /// `JOBS_EMBEDDED`: also run a worker inside `serve` (development, tiny deployments).
    pub embedded: bool,
    /// `JOBS_QUEUES`: `name:slots[:max_running]`, comma separated.
    pub queues: String,
    pub poll_ms: u64,
    /// `JOBS_NOTIFY`: NOTIFY on enqueue and LISTEN in workers.
    pub notify: bool,
    /// `JOBS_DB_POOL`: the worker's own pool (claims, heartbeats, handlers).
    pub db_pool: u32,
    pub shutdown_timeout_s: u64,
    pub keep_succeeded_h: u64,
    pub keep_failed_h: u64,
    /// `JOBS_METRICS_ADDR`: `/metrics`, `/healthz`, `/readyz` of `worker`.
    pub metrics_addr: SocketAddr,
    /// `JOBS_SMTP_URL`, e.g. `smtp://user:pass@host:587?tls=required`. Unset: no emails.
    pub smtp_url: Option<Secret>,
    pub smtp_timeout_s: u64,
    pub email_from: String,
    /// Right-hand side of the stable `Message-ID`.
    pub email_domain: String,
    pub webhooks: Vec<WebhookEndpoint>,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedded: false,
            queues: "email:4,webhooks:8".to_owned(),
            poll_ms: 1000,
            notify: true,
            db_pool: 16,
            shutdown_timeout_s: 25,
            keep_succeeded_h: 24,
            keep_failed_h: 14 * 24,
            metrics_addr: SocketAddr::from(([0, 0, 0, 0], 9300)),
            smtp_url: None,
            smtp_timeout_s: 30,
            email_from: "library@localhost".to_owned(),
            email_domain: "library.local".to_owned(),
            webhooks: Vec::new(),
        }
    }
}

const MAX_WEBHOOK_ENDPOINTS: usize = 10;

/// Ten years; keeps `hours * 3600` far from overflow.
const MAX_KEEP_H: u64 = 87_600;

impl JobsConfig {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        // An empty value is the same as an unset one (compose writes `VAR:` as "").
        let get = |key: &str| get(key).filter(|v| !v.trim().is_empty());
        let d = Self::default();
        let positive = |key: &str, default: u64| -> anyhow::Result<u64> {
            let n: u64 = parse_or(&get, key, default)?;
            if n == 0 {
                bail!("{key} must be greater than zero");
            }
            Ok(n)
        };
        let db_pool = u32::try_from(positive("JOBS_DB_POOL", u64::from(d.db_pool))?)
            .context("JOBS_DB_POOL is too large")?;
        let enabled = parse_or(&get, "JOBS_ENABLED", d.enabled)?;
        let embedded = parse_or(&get, "JOBS_EMBEDDED", d.embedded)?;
        // Fail fast: an embedded worker that silently never starts would leave jobs waiting.
        if embedded && !enabled {
            bail!("JOBS_EMBEDDED=true needs JOBS_ENABLED=true");
        }
        Ok(Self {
            enabled,
            embedded,
            queues: get("JOBS_QUEUES").unwrap_or(d.queues),
            poll_ms: positive("JOBS_POLL_MS", d.poll_ms)?,
            notify: parse_or(&get, "JOBS_NOTIFY", d.notify)?,
            db_pool,
            shutdown_timeout_s: positive("JOBS_SHUTDOWN_TIMEOUT_S", d.shutdown_timeout_s)?,
            keep_succeeded_h: keep_hours(&positive, "JOBS_KEEP_SUCCEEDED_H", d.keep_succeeded_h)?,
            keep_failed_h: keep_hours(&positive, "JOBS_KEEP_FAILED_H", d.keep_failed_h)?,
            metrics_addr: parse_or(&get, "JOBS_METRICS_ADDR", d.metrics_addr)?,
            smtp_url: get("JOBS_SMTP_URL").map(Secret::new),
            smtp_timeout_s: positive("JOBS_SMTP_TIMEOUT_S", d.smtp_timeout_s)?,
            email_from: get("JOBS_EMAIL_FROM").unwrap_or(d.email_from),
            email_domain: get("JOBS_EMAIL_DOMAIN").unwrap_or(d.email_domain),
            webhooks: webhook_endpoints(&get)?,
        })
    }
}

fn keep_hours(
    positive: &impl Fn(&str, u64) -> anyhow::Result<u64>,
    key: &str,
    default: u64,
) -> anyhow::Result<u64> {
    let hours = positive(key, default)?;
    if hours > MAX_KEEP_H {
        bail!("{key} must be at most {MAX_KEEP_H} hours (10 years), got {hours}");
    }
    Ok(hours)
}

fn webhook_endpoints(
    get: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Vec<WebhookEndpoint>> {
    let Some(raw) = get("JOBS_WEBHOOK_ENDPOINTS") else {
        return Ok(Vec::new());
    };
    let urls: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .collect();
    if urls.len() > MAX_WEBHOOK_ENDPOINTS {
        bail!(
            "JOBS_WEBHOOK_ENDPOINTS lists {} endpoints; at most {MAX_WEBHOOK_ENDPOINTS}",
            urls.len()
        );
    }
    urls.into_iter()
        .enumerate()
        .map(|(i, url)| {
            let ok = (url.starts_with("https://") || url.starts_with("http://"))
                && url.len() <= 2048
                && !url.chars().any(char::is_whitespace);
            if !ok {
                // Names the position only: the URL may carry a token.
                bail!(
                    "JOBS_WEBHOOK_ENDPOINTS: endpoint {} is not an http(s) URL",
                    i + 1
                );
            }
            let var = format!("JOBS_WEBHOOK_SECRET_{}", i + 1);
            let secret = get(&var)
                .with_context(|| format!("{var} is required for webhook endpoint {}", i + 1))?;
            decode_secret(&secret).with_context(|| format!("{var} is invalid"))?;
            Ok(WebhookEndpoint {
                url: url.to_owned(),
                secret: Secret::new(secret),
            })
        })
        .collect()
}

/// The key bytes of a `whsec_<base64>` secret (24 to 64 bytes).
pub fn decode_secret(secret: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = secret
        .strip_prefix("whsec_")
        .context("the secret must start with `whsec_`")?;
    // The decoder's own error names the offending symbol and offset: leave it out.
    let key = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!("the part after `whsec_` must be base64"))?;
    if !(24..=64).contains(&key.len()) {
        bail!("the key must be 24 to 64 bytes, got {}", key.len());
    }
    Ok(key)
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

    // "whsec_" + base64 of 32 bytes 0..32. Test value, local only.
    const SECRET: &str = "whsec_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    #[test]
    fn disabled_by_default_with_documented_defaults() {
        let c = JobsConfig::from_lookup(&lookup(&[])).unwrap();
        assert_eq!(c, JobsConfig::default());
        assert!(!c.enabled && !c.embedded && c.notify);
        assert_eq!(c.queues, "email:4,webhooks:8");
        assert_eq!((c.poll_ms, c.db_pool, c.shutdown_timeout_s), (1000, 16, 25));
        assert_eq!(
            (c.keep_succeeded_h, c.keep_failed_h, c.smtp_timeout_s),
            (24, 336, 30)
        );
        assert_eq!(c.metrics_addr.to_string(), "0.0.0.0:9300");
        assert!(c.smtp_url.is_none() && c.webhooks.is_empty());
    }

    #[test]
    fn empty_values_count_as_unset() {
        let c = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_SMTP_URL", ""),
            ("JOBS_WEBHOOK_ENDPOINTS", ""),
        ]))
        .unwrap();
        assert!(c.smtp_url.is_none() && c.webhooks.is_empty());
    }

    #[test]
    fn each_webhook_endpoint_needs_its_numbered_secret() {
        let ok = JobsConfig::from_lookup(&lookup(&[
            (
                "JOBS_WEBHOOK_ENDPOINTS",
                "https://a.example/hook, http://b.example/x",
            ),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
            ("JOBS_WEBHOOK_SECRET_2", SECRET),
        ]))
        .unwrap();
        assert_eq!(
            ok.webhooks
                .iter()
                .map(|w| w.url.as_str())
                .collect::<Vec<_>>(),
            ["https://a.example/hook", "http://b.example/x"]
        );
        let err = JobsConfig::from_lookup(&lookup(&[
            (
                "JOBS_WEBHOOK_ENDPOINTS",
                "https://a.example/hook,https://b.example/x",
            ),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("JOBS_WEBHOOK_SECRET_2"), "{err}");
    }

    #[test]
    fn bad_endpoints_and_secrets_are_refused_by_name() {
        for (url, secret, var) in [
            ("ftp://a.example/x", SECRET, "JOBS_WEBHOOK_ENDPOINTS"),
            (
                "https://a.example/x",
                "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
                "JOBS_WEBHOOK_SECRET_1",
            ),
            (
                "https://a.example/x",
                "whsec_not base64!",
                "JOBS_WEBHOOK_SECRET_1",
            ),
            ("https://a.example/x", "whsec_AAEC", "JOBS_WEBHOOK_SECRET_1"),
        ] {
            let err = JobsConfig::from_lookup(&lookup(&[
                ("JOBS_WEBHOOK_ENDPOINTS", url),
                ("JOBS_WEBHOOK_SECRET_1", secret),
            ]))
            .unwrap_err();
            assert!(err.to_string().contains(var), "{url} {secret}: {err}");
        }
    }

    #[test]
    fn numbers_are_validated_with_the_variable_name() {
        for (var, bad) in [
            ("JOBS_POLL_MS", "0"),
            ("JOBS_DB_POOL", "0"),
            ("JOBS_SHUTDOWN_TIMEOUT_S", "x"),
            ("JOBS_SMTP_TIMEOUT_S", "0"),
        ] {
            let err = JobsConfig::from_lookup(&lookup(&[(var, bad)])).unwrap_err();
            assert!(err.to_string().contains(var), "{var}: {err}");
        }
    }

    #[test]
    fn embedded_needs_jobs_enabled() {
        let err = JobsConfig::from_lookup(&lookup(&[("JOBS_EMBEDDED", "true")])).unwrap_err();
        assert!(err.to_string().contains("JOBS_EMBEDDED"), "{err}");
        let ok = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_EMBEDDED", "true"),
            ("JOBS_ENABLED", "true"),
        ]))
        .unwrap();
        assert!(ok.enabled && ok.embedded);
    }

    #[test]
    fn retention_is_capped_so_hours_to_seconds_cannot_overflow() {
        for var in ["JOBS_KEEP_SUCCEEDED_H", "JOBS_KEEP_FAILED_H"] {
            let ok = JobsConfig::from_lookup(&lookup(&[(var, "87600")])).unwrap();
            assert!(ok.keep_succeeded_h <= 87_600 && ok.keep_failed_h <= 87_600);
            for bad in ["87601", "18446744073709551615"] {
                let err = JobsConfig::from_lookup(&lookup(&[(var, bad)])).unwrap_err();
                assert!(err.to_string().contains(var), "{var}={bad}: {err}");
            }
        }
    }

    #[test]
    fn errors_and_debug_never_echo_webhook_urls_or_secret_bytes() {
        let token_url = "https://user:pw123@hooks.example/services/T0/B1/tok3nvalue?key=q7";
        // A bad URL (spaces) and a missing secret both name the position, not the URL.
        for (endpoints, secret) in [
            ("ftp://user:pw123@hooks.example/tok3nvalue", Some(SECRET)),
            ("https://hooks.example/tok3nvalue x", Some(SECRET)),
            (token_url, None),
            (token_url, Some("whsec_tok3n value!")),
        ] {
            let mut pairs = vec![("JOBS_WEBHOOK_ENDPOINTS", endpoints)];
            pairs.extend(secret.map(|s| ("JOBS_WEBHOOK_SECRET_1", s)));
            let err = JobsConfig::from_lookup(&lookup(&pairs)).unwrap_err();
            let shown = format!("{err:#} {err:?}");
            for leak in ["pw123", "tok3n", "q7", "offset", "symbol"] {
                assert!(!shown.contains(leak), "{leak} leaked: {shown}");
            }
        }
        let c = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_WEBHOOK_ENDPOINTS", token_url),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
        ]))
        .unwrap();
        let shown = format!("{c:?}");
        assert!(shown.contains("https://hooks.example"), "{shown}");
        for leak in ["pw123", "tok3n", "q7", "services"] {
            assert!(!shown.contains(leak), "{leak} leaked: {shown}");
        }
    }

    #[test]
    fn debug_output_never_shows_secrets() {
        let c = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_SMTP_URL", "smtp://user:hunter2@mail.example:587"),
            ("JOBS_WEBHOOK_ENDPOINTS", "https://a.example/hook"),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
        ]))
        .unwrap();
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("AAECAwQF"),
            "{shown}"
        );
        assert_eq!(
            decode_secret(SECRET).unwrap(),
            (0u8..32).collect::<Vec<_>>()
        );
    }
}
