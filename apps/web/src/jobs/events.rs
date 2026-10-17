//! Loan events: what borrowing or returning a book tells the outside world
//! (spec §15.1).

use library_jobs::{EnqueueError, Enqueuer};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use super::loan_email::LoanEmail;
use super::webhook::{Webhook, WebhookEvent, endpoint_id};
use crate::jobs_config::JobsConfig;
use crate::loans::LoanDetails;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoanEventKind {
    Borrowed,
    Returned,
}

impl LoanEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Borrowed => "borrowed",
            Self::Returned => "returned",
        }
    }
}

/// `loan.<id>.<borrowed|returned>`: the same for every job and attempt of one
/// event, so receivers can de-duplicate on it.
pub fn event_id(loan_id: i64, kind: LoanEventKind) -> String {
    format!("loan.{loan_id}.{}", kind.as_str())
}

/// Enqueues the jobs of a loan event. The repository calls it inside its
/// transaction, just before commit: the jobs exist exactly when the loan change
/// does (the outbox, spec §6.2).
#[derive(Debug, Clone)]
pub struct LoanEvents {
    enqueuer: Enqueuer,
    email: bool,
    /// [`endpoint_id`]s, never URLs: a URL may carry a token and jobs are stored.
    endpoints: Vec<String>,
}

impl LoanEvents {
    /// `None` with jobs disabled. Emails only with an SMTP URL, webhooks one per
    /// configured endpoint (they fail and retry independently).
    pub fn from_config(jobs: &JobsConfig) -> Option<Self> {
        jobs.enabled.then(|| {
            // Two entries with one URL are one endpoint: one job, not a duplicate.
            let mut endpoints: Vec<String> = Vec::new();
            for entry in &jobs.webhooks {
                let id = endpoint_id(&entry.url);
                if !endpoints.contains(&id) {
                    endpoints.push(id);
                }
            }
            Self {
                enqueuer: Enqueuer::new(jobs.notify),
                email: jobs.smtp_url.is_some(),
                endpoints,
            }
        })
    }

    /// `conn` is the caller's open transaction.
    pub async fn record(
        &self,
        conn: &mut PgConnection,
        kind: LoanEventKind,
        loan: &LoanDetails,
    ) -> Result<(), EnqueueError> {
        if self.email {
            self.enqueuer
                .enqueue(
                    &mut *conn,
                    &LoanEmail {
                        loan_id: loan.id,
                        event: kind,
                    },
                )
                .await?;
        }
        if !self.endpoints.is_empty() {
            let event = WebhookEvent::loan(kind, loan);
            for endpoint in &self.endpoints {
                let job = Webhook {
                    endpoint: endpoint.clone(),
                    event: event.clone(),
                };
                self.enqueuer.enqueue(&mut *conn, &job).await?;
            }
        }
        Ok(())
    }
}
