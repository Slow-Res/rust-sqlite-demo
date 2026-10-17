//! `loan_email.v1`: tell the member about a borrow or a return (spec §15.2).

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
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

/// SMTP sender shared by every email job of a process. One connection per email:
/// a send that is cancelled mid-transaction (the timeout in [`Mailer::send`]) must
/// never leave a connection behind whose next reply belongs to the old message,
/// and lettre's pool would reuse it. Loan emails are rare enough not to need reuse.
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
        if sends_credentials_in_clear(smtp_url) {
            bail!(
                "JOBS_SMTP_URL carries a username and password but does not require TLS: \
                 use smtps://, or add ?tls=required for STARTTLS"
            );
        }
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
        // Fixed texts only: job errors are stored, listed by `jobs show` and kept for
        // days, and must not hold a member's address or name (the loan id is in the
        // job's args).
        let address: Address = to_email
            .parse()
            .map_err(|_| JobError::permanent("member address is not valid"))?;
        Message::builder()
            .message_id(Some(format!("<loan_email.v1.{job_id}@{}>", self.domain)))
            .from(self.from.clone())
            .to(Mailbox::new(Some(to_name.to_owned()), address))
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(|_| JobError::permanent("the email could not be built"))
    }

    pub async fn send(&self, message: Message) -> Result<(), JobError> {
        // lettre's own timeout covers only the TCP connect of its async transport,
        // not the replies: a server that accepts and then says nothing would hold
        // the attempt until the job deadline. So bound the whole send here. Dropping
        // the future closes its connection (there is no pool, see `Mailer`).
        match tokio::time::timeout(self.timeout, self.transport.send(message)).await {
            Ok(sent) => sent.map(drop).map_err(classify),
            Err(_) => Err(JobError::retry("SMTP server did not answer in time")),
        }
    }
}

/// A URL with credentials (`user:pass@`) must not use a connection that may stay in
/// clear text: only `smtps://` (TLS from the first byte) and `?tls=required` or
/// `?tls=wrapper` guarantee encryption (`smtp://` alone, or `?tls=opportunistic`, falls
/// back to plain text when the server or a man in the middle says so).
fn sends_credentials_in_clear(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false; // not a URL: lettre refuses it
    };
    let (rest, _fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (before_query, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = before_query.split('/').next().unwrap_or_default();
    if !authority.contains('@') || scheme.eq_ignore_ascii_case("smtps") {
        return false;
    }
    !query
        .split('&')
        .any(|p| p.eq_ignore_ascii_case("tls=required") || p.eq_ignore_ascii_case("tls=wrapper"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_need_tls() {
        for clear in [
            "smtp://user:pw@mail.example:587",
            "smtp://user@mail.example",
            "smtp://user:pw@mail.example:587?tls=opportunistic",
            "SMTP://user:pw@mail.example/?x=tls=required",
            "smtp://user:pw@mail.example#?tls=required",
        ] {
            assert!(sends_credentials_in_clear(clear), "{clear}");
        }
        for fine in [
            "smtp://127.0.0.1:55025",
            "smtp://mail.example:25?tls=opportunistic",
            "smtps://user:pw@mail.example:465",
            "SMTPS://user:pw@mail.example",
            "smtp://user:pw@mail.example:587?tls=required",
            "smtp://user:pw@mail.example:587?a=b&tls=required",
            "smtp://user:pw@mail.example:465?tls=wrapper",
            "smtp://mail.example/path@x",
        ] {
            assert!(!sends_credentials_in_clear(fine), "{fine}");
        }
    }

    #[test]
    fn plain_credentials_are_refused_without_echoing_the_url() {
        let Err(err) = Mailer::new(
            "smtp://mailer:hunter2secret@mail.example:587",
            "l@library.test",
            "library.test",
            Duration::from_secs(1),
        ) else {
            panic!("credentials over a connection that may be plain must be refused");
        };
        let text = format!("{err:#} {err:?}");
        assert!(text.contains("TLS"), "{text}");
        for leak in ["hunter2secret", "mailer", "mail.example"] {
            assert!(!text.contains(leak), "{leak} leaked: {text}");
        }
        for ok in [
            "smtp://127.0.0.1:55025",
            "smtps://mailer:pw@mail.example:465",
            "smtp://mailer:pw@mail.example:587?tls=required",
        ] {
            assert!(
                Mailer::new(ok, "l@library.test", "library.test", Duration::from_secs(1)).is_ok(),
                "{ok}"
            );
        }
    }
}
