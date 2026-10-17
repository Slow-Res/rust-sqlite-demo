//! Readiness without any web framework (spec §17.3, §23). `http` serves it as
//! `/readyz`; an app on another framework calls it from its own handler.

use std::fmt;
use std::time::Duration;

use sqlx::PgPool;

use crate::heartbeat::Health;

/// Longest wait for the database's answer.
const DB_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a worker process is not ready to be counted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    /// The registry row was deleted: the process stopped working and must exit.
    Fenced,
    /// The worker has not completed a heartbeat since it started.
    NoHeartbeatYet,
    /// The last successful heartbeat is older than the allowed age.
    StaleHeartbeat(Duration),
    /// The database did not answer `SELECT 1` within two seconds.
    DatabaseUnreachable,
}

impl fmt::Display for NotReady {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fenced => f.write_str("self-fenced: the registry row was deleted"),
            Self::NoHeartbeatYet => f.write_str("no heartbeat yet"),
            Self::StaleHeartbeat(age) => write!(f, "last heartbeat {age:?} ago"),
            Self::DatabaseUnreachable => f.write_str("database unreachable"),
        }
    }
}

impl std::error::Error for NotReady {}

/// Ready: not fenced, a heartbeat within `stale_after` (three heartbeat
/// periods is the usual choice), and the database answers within 2 s.
pub async fn ready(health: &Health, pool: &PgPool, stale_after: Duration) -> Result<(), NotReady> {
    if health.is_fenced() {
        return Err(NotReady::Fenced);
    }
    match health.last_beat_age() {
        None => return Err(NotReady::NoHeartbeatYet),
        Some(age) if age > stale_after => return Err(NotReady::StaleHeartbeat(age)),
        Some(_) => {}
    }
    let ping = tokio::time::timeout(DB_TIMEOUT, sqlx::query("SELECT 1").execute(pool)).await;
    if matches!(ping, Ok(Ok(_))) {
        Ok(())
    } else {
        Err(NotReady::DatabaseUnreachable)
    }
}
