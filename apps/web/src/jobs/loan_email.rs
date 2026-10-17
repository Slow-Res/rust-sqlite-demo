//! `loan_email.v1`: tell the member about a borrow or a return (spec §15.2).

use std::time::Duration;

use anyhow::{Context, anyhow};
use chrono::DateTime;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use library_jobs::{Job, JobContext, JobError, JobOptions};
use serde::{Deserialize, Serialize};

use super::events::{LoanEventKind, event_id};
use crate::jobs_config::JobsConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoanEmail {
    pub loan_id: i64,
    pub event: LoanEventKind,
}

/// What the email says, read when it is sent: it reflects the loan as it is now.
#[derive(sqlx::FromRow)]
struct LoanRow {
    member_name: String,
    member_email: String,
    book_title: String,
    due_at: i64,
    returned_at: Option<i64>,
}

impl Job for LoanEmail {
    const KIND: &'static str = "loan_email.v1";
    const QUEUE: &'static str = "email";

    fn options() -> JobOptions {
        // The SMTP timeout is 30 s by default; two minutes leaves room for a slow server.
        JobOptions {
            deadline: Duration::from_secs(120),
            ..JobOptions::default()
        }
    }

    fn unique_key(&self) -> Option<String> {
        Some(event_id(self.loan_id, self.event))
    }

    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        let mailer = ctx.require::<Mailer>()?;
        let row: Option<LoanRow> = sqlx::query_as(
            "SELECT m.name AS member_name, m.email AS member_email, b.title AS book_title, l.due_at, l.returned_at
             FROM loans l JOIN members m ON m.id = l.member_id JOIN books b ON b.id = l.book_id
             WHERE l.id = $1",
        )
        .bind(self.loan_id)
        .fetch_optional(ctx.pool())
        .await?;
        let Some(row) = row else {
            tracing::info!(
                loan_id = self.loan_id,
                "loan_email: the loan is gone; nothing to send"
            );
            return Ok(());
        };
        if self.event == LoanEventKind::Borrowed && row.returned_at.is_some() {
            tracing::info!(
                loan_id = self.loan_id,
                "loan_email: already returned; the borrow email is stale"
            );
            return Ok(());
        }
        let (subject, body) = compose(self.event, &row);
        let message = mailer.message(
            ctx.id(),
            &row.member_name,
            &row.member_email,
            &subject,
            body,
        )?;
        mailer.send(message).await
    }
}

fn date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).map_or_else(
        || "unknown".to_owned(),
        |d| d.format("%Y-%m-%d").to_string(),
    )
}

fn compose(event: LoanEventKind, row: &LoanRow) -> (String, String) {
    let (name, title) = (&row.member_name, &row.book_title);
    match event {
        LoanEventKind::Borrowed => (
            format!("You borrowed \"{title}\""),
            format!(
                "Hello {name},\n\nyou borrowed \"{title}\". Please return it by {}.\n\nThe library\n",
                date(row.due_at)
            ),
        ),
        LoanEventKind::Returned => (
            format!("You returned \"{title}\""),
            format!("Hello {name},\n\nthank you for returning \"{title}\".\n\nThe library\n"),
        ),
    }
}

/// SMTP sender shared by every email job of a process (pooled connections).
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    domain: String,
    timeout: Duration,
}

impl Mailer {
    /// `smtp_url`: `smtp://host:port` (plain), `smtp://user:pass@host:587?tls=required`
    /// (STARTTLS) or `smtps://…` (TLS). `timeout` bounds connecting and the whole send.
    pub fn new(
        smtp_url: &str,
        from: &str,
        domain: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        // The URL may carry credentials, so no part of it (nor of lettre's error
        // about it) goes into the message.
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(smtp_url)
            .map_err(|_| anyhow!("JOBS_SMTP_URL is not a valid SMTP URL"))?
            .timeout(Some(timeout))
            .build();
        let from = from
            .parse()
            .with_context(|| format!("JOBS_EMAIL_FROM `{from}` is not an email address"))?;
        Ok(Self {
            transport,
            from,
            domain: domain.to_owned(),
            timeout,
        })
    }

    /// `None` when no SMTP URL is configured: the email kind is then not registered.
    pub fn from_config(jobs: &JobsConfig) -> anyhow::Result<Option<Self>> {
        jobs.smtp_url
            .as_ref()
            .map(|url| {
                Self::new(
                    url.expose(),
                    &jobs.email_from,
                    &jobs.email_domain,
                    Duration::from_secs(jobs.smtp_timeout_s),
                )
            })
            .transpose()
    }

    /// The `Message-ID` comes from the job id, so every attempt sends the same one
    /// and a receiver can drop a resend after a crash (spec §15.2).
    pub fn message(
        &self,
        job_id: i64,
        to_name: &str,
        to_email: &str,
        subject: &str,
        body: String,
    ) -> Result<Message, JobError> {
        let address: Address = to_email
            .parse()
            .map_err(|err| JobError::permanent(format!("member address `{to_email}`: {err}")))?;
        Message::builder()
            .message_id(Some(format!("<loan_email.v1.{job_id}@{}>", self.domain)))
            .from(self.from.clone())
            .to(Mailbox::new(Some(to_name.to_owned()), address))
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(JobError::permanent)
    }

    pub async fn send(&self, message: Message) -> Result<(), JobError> {
        // lettre's own timeout covers only the TCP connect of its async transport,
        // not the replies: a server that accepts and then says nothing would hold
        // the attempt until the job deadline. So bound the whole send here.
        match tokio::time::timeout(self.timeout, self.transport.send(message)).await {
            Ok(sent) => sent.map(drop).map_err(classify),
            Err(_) => Err(JobError::retry("SMTP server did not answer in time")),
        }
    }
}

/// 5xx replies are permanent (bad mailbox, rejected sender). Everything else
/// (4xx, timeouts, refused connections, TLS errors) may pass on a later attempt.
///
/// The text is fixed, never lettre's own: its errors can name the server or carry
/// the server's reply, and job errors are stored and logged (spec §9.1).
fn classify(err: lettre::transport::smtp::Error) -> JobError {
    let code = err.status().map(|code| code.to_string());
    if err.is_permanent() {
        let code = code.as_deref().unwrap_or("5xx");
        JobError::permanent(format!("SMTP server refused the message ({code})"))
    } else if err.is_transient() {
        let code = code.as_deref().unwrap_or("4xx");
        JobError::retry(format!("SMTP server asked to try again later ({code})"))
    } else if err.is_timeout() {
        JobError::retry("SMTP server did not answer in time")
    } else if err.is_tls() {
        JobError::retry("SMTP TLS error")
    } else {
        JobError::retry("SMTP connection or protocol error")
    }
}
