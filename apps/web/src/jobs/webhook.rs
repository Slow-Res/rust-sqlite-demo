//! `webhook.v1`: POST a loan event to an operator-configured endpoint, signed per
//! the Standard Webhooks spec (spec §15.3).
//!
//! Receivers often carry a token in the URL path, query or userinfo, so a URL is
//! never stored, logged or put in an error: jobs and messages name an endpoint by
//! its [`endpoint_id`].

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use library_jobs::{Job, JobContext, JobError, JobOptions};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::events::{LoanEventKind, event_id};
use crate::jobs_config::{JobsConfig, WebhookEndpoint, decode_secret, redacted_url};
use crate::loans::LoanDetails;

/// Response bodies are read up to this size (so the connection can be reused), then dropped.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// The longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// A stable, secret-free name for a configured endpoint: the redacted
/// `scheme://host[:port]`, `#`, and the first 12 hex characters of the SHA-256 of
/// the full URL (so two endpoints on one host stay apart, and a changed URL is a
/// different endpoint).
pub fn endpoint_id(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("{}#{}", redacted_url(url), hex::encode(&digest[..6]))
}

/// The JSON body: `{id, type, occurred_at, data}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub occurred_at: DateTime<Utc>,
    pub data: Value,
}

impl WebhookEvent {
    /// Captured at enqueue: an event is a fact at a point in time, unlike the email.
    pub fn loan(kind: LoanEventKind, loan: &LoanDetails) -> Self {
        let occurred_at = match kind {
            LoanEventKind::Borrowed => loan.borrowed_at,
            LoanEventKind::Returned => loan.returned_at.unwrap_or(loan.borrowed_at),
        };
        Self {
            id: event_id(loan.id, kind),
            kind: format!("loan.{}", kind.as_str()),
            occurred_at,
            data: json!({ "loan": {
                "id": loan.id,
                "book_id": loan.book_id,
                "book_title": loan.book_title,
                "member_id": loan.member_id,
                "member_name": loan.member_name,
                "borrowed_at": loan.borrowed_at,
                "due_at": loan.due_at,
                "returned_at": loan.returned_at,
            }}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Webhook {
    /// The [`endpoint_id`] of one of `JOBS_WEBHOOK_ENDPOINTS`, which must still be
    /// configured when the job runs. Never the URL itself.
    pub endpoint: String,
    pub event: WebhookEvent,
}

impl Job for Webhook {
    const KIND: &'static str = "webhook.v1";
    const QUEUE: &'static str = "webhooks";

    fn options() -> JobOptions {
        JobOptions {
            deadline: Duration::from_secs(60),
            ..JobOptions::default()
        }
    }

    fn unique_key(&self) -> Option<String> {
        Some(format!("{} {}", self.event.id, self.endpoint))
    }

    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        ctx.require::<WebhookSender>()?
            .deliver(&self.endpoint, &self.event)
            .await
    }
}

/// Standard Webhooks: `v1,` + base64(HMAC-SHA256(key, "{id}.{timestamp}.{body}")).
pub fn sign(key: &[u8], id: &str, timestamp: i64, body: &[u8]) -> Result<String, JobError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| JobError::permanent("the webhook key was refused by HMAC"))?;
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

/// One HTTP client and the configured endpoints (by [`endpoint_id`]), shared by
/// every webhook job of a process.
pub struct WebhookSender {
    client: reqwest::Client,
    endpoints: HashMap<String, Target>,
}

/// Where to POST and with which key.
struct Target {
    url: String,
    key: Vec<u8>,
}

impl WebhookSender {
    pub fn new(
        endpoints: &[WebhookEndpoint],
        connect_timeout: Duration,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .timeout(timeout)
            .user_agent(concat!(
                "library-web/",
                env!("CARGO_PKG_VERSION"),
                " webhooks"
            ))
            .build()
            .context("building the webhook HTTP client")?;
        let endpoints = endpoints
            .iter()
            .map(|e| {
                let id = endpoint_id(&e.url);
                let key =
                    decode_secret(e.secret.expose()).with_context(|| format!("secret of {id}"))?;
                Ok((
                    id,
                    Target {
                        url: e.url.clone(),
                        key,
                    },
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { client, endpoints })
    }

    /// `None` when no endpoint is configured: the webhook kind is then not registered.
    pub fn from_config(jobs: &JobsConfig) -> anyhow::Result<Option<Self>> {
        if jobs.webhooks.is_empty() {
            return Ok(None);
        }
        Self::new(
            &jobs.webhooks,
            Duration::from_secs(5),
            Duration::from_secs(15),
        )
        .map(Some)
    }

    /// POST `event` to the endpoint named by `endpoint` (an [`endpoint_id`]).
    pub async fn deliver(&self, endpoint: &str, event: &WebhookEvent) -> Result<(), JobError> {
        let Some(target) = self.endpoints.get(endpoint) else {
            return Err(JobError::permanent(format!(
                "endpoint {endpoint} is no longer configured"
            )));
        };
        let body = serde_json::to_vec(event).map_err(JobError::permanent)?;
        // The receiver compares this with its own clock to refuse replays, so it is
        // the send time; it is never stored or compared by us.
        let timestamp = Utc::now().timestamp();
        let signature = sign(&target.key, &event.id, timestamp, &body)?;
        let response = self
            .client
            .post(&target.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("webhook-id", &event.id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(body)
            .send()
            .await
            // reqwest errors embed the request URL, token and all.
            .map_err(|err| JobError::retry(format!("POST {endpoint}: {}", err.without_url())))?;
        let status = response.status();
        let retry_after = retry_after(response.headers());
        drain(response).await;
        outcome(status, retry_after, endpoint)
    }
}

async fn drain(mut response: reqwest::Response) {
    let mut read = 0;
    while read < MAX_RESPONSE_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => read += chunk.len(),
            _ => break,
        }
    }
}

/// `Retry-After` in seconds; HTTP dates fall back to the normal backoff.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// 2xx → done; 408, 429 and 5xx → retry; redirects and other 4xx → permanent.
fn outcome(
    status: StatusCode,
    retry_after: Option<Duration>,
    endpoint: &str,
) -> Result<(), JobError> {
    if status.is_success() {
        return Ok(());
    }
    let what = format!("POST {endpoint}: HTTP {}", status.as_u16());
    if status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        return Err(match retry_after {
            Some(delay) => JobError::RetryAfter(delay, what),
            None => JobError::Retry(what),
        });
    }
    if status.is_redirection() {
        return Err(JobError::Permanent(format!(
            "{what} (redirects are not followed)"
        )));
    }
    Err(JobError::Permanent(what))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_the_standard_webhooks_reference_vector() {
        let key = STANDARD.decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        let sig = sign(
            &key,
            "msg_p5jXN8AQM9LWM0D4loKWxJek",
            1_614_265_330,
            br#"{"test": 2432232314}"#,
        )
        .unwrap();
        assert_eq!(sig, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
    }

    #[test]
    fn statuses_map_to_outcomes() {
        let ok = |s: u16| outcome(StatusCode::from_u16(s).unwrap(), None, "e");
        assert!(ok(200).is_ok() && ok(204).is_ok());
        for retry in [408, 429, 500, 502, 503] {
            assert!(matches!(ok(retry), Err(JobError::Retry(_))), "{retry}");
        }
        for permanent in [301, 302, 400, 401, 404, 410, 422] {
            assert!(
                matches!(ok(permanent), Err(JobError::Permanent(_))),
                "{permanent}"
            );
        }
        let later = outcome(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(7)),
            "e",
        );
        assert!(matches!(later, Err(JobError::RetryAfter(d, _)) if d == Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_takes_seconds_and_caps_them() {
        let h = |v: &str| {
            let mut m = reqwest::header::HeaderMap::new();
            m.insert(reqwest::header::RETRY_AFTER, v.parse().unwrap());
            retry_after(&m)
        };
        assert_eq!(h("7"), Some(Duration::from_secs(7)));
        assert_eq!(h("999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(
            h("Wed, 21 Oct 2015 07:28:00 GMT"),
            None,
            "dates fall back to the normal backoff"
        );
    }
}
