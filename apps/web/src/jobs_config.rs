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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEndpoint {
    pub url: String,
    /// `whsec_<base64 key>`, the Standard Webhooks format.
    pub secret: Secret,
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
        Ok(Self {
            enabled: parse_or(&get, "JOBS_ENABLED", d.enabled)?,
            embedded: parse_or(&get, "JOBS_EMBEDDED", d.embedded)?,
            queues: get("JOBS_QUEUES").unwrap_or(d.queues),
            poll_ms: positive("JOBS_POLL_MS", d.poll_ms)?,
            notify: parse_or(&get, "JOBS_NOTIFY", d.notify)?,
            db_pool,
            shutdown_timeout_s: positive("JOBS_SHUTDOWN_TIMEOUT_S", d.shutdown_timeout_s)?,
            keep_succeeded_h: positive("JOBS_KEEP_SUCCEEDED_H", d.keep_succeeded_h)?,
            keep_failed_h: positive("JOBS_KEEP_FAILED_H", d.keep_failed_h)?,
            metrics_addr: parse_or(&get, "JOBS_METRICS_ADDR", d.metrics_addr)?,
            smtp_url: get("JOBS_SMTP_URL").map(Secret::new),
            smtp_timeout_s: positive("JOBS_SMTP_TIMEOUT_S", d.smtp_timeout_s)?,
            email_from: get("JOBS_EMAIL_FROM").unwrap_or(d.email_from),
            email_domain: get("JOBS_EMAIL_DOMAIN").unwrap_or(d.email_domain),
            webhooks: webhook_endpoints(&get)?,
        })
    }
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
                bail!("JOBS_WEBHOOK_ENDPOINTS: `{url}` is not an http(s) URL");
            }
            let var = format!("JOBS_WEBHOOK_SECRET_{}", i + 1);
            let secret = get(&var)
                .with_context(|| format!("{var} is required for webhook endpoint {url}"))?;
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
    let key = STANDARD
        .decode(encoded)
        .context("the part after `whsec_` must be base64")?;
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
