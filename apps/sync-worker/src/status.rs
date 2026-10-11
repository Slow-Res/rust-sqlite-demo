//! `sync-worker status`: where this node's copy in Postgres stands (RUNBOOK).

use sqlx::Connection;

use crate::changes::{self, Checkpoint, Pending};
use crate::config::Config;
use crate::metrics::{lag_seconds, now_ms};
use crate::mirror;

pub async fn status(config: &Config) -> anyhow::Result<String> {
    let sqlite = sqlite_setup::open_pool(&config.database_url, 1, false).await?;
    let mut conn = mirror::connect_with_timeouts(&config.postgres).await?;
    let checkpoint = mirror::load_checkpoint(&mut conn, &config.node_id).await;
    conn.close().await.ok();
    let text = match checkpoint? {
        None => format!(
            "node: {}\nnot synced yet: Postgres has no checkpoint for this node\n",
            config.node_id
        ),
        Some(checkpoint) => {
            let pending = changes::pending(&sqlite, checkpoint.last_seq).await?;
            render(&config.node_id, &checkpoint, pending, now_ms())
        }
    };
    sqlite.close().await;
    Ok(text)
}

pub fn render(node_id: &str, checkpoint: &Checkpoint, pending: Pending, now_ms: i64) -> String {
    let waiting = match pending.oldest_ms {
        None => "nothing".to_owned(),
        Some(oldest) => format!(
            "{} changes, oldest {:.1} s old",
            pending.count,
            lag_seconds(Some(oldest), now_ms)
        ),
    };
    format!(
        "node: {node_id}\nepoch: {}\nlast shipped seq: {}\nwaiting to ship: {waiting}\n",
        checkpoint.epoch, checkpoint.last_seq
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_position_and_backlog() {
        let checkpoint = Checkpoint {
            epoch: 3,
            last_seq: 1234,
            last_token: None,
        };
        assert_eq!(
            render(
                "node1",
                &checkpoint,
                Pending {
                    count: 5,
                    oldest_ms: Some(9_000)
                },
                10_000
            ),
            "node: node1\nepoch: 3\nlast shipped seq: 1234\nwaiting to ship: 5 changes, oldest 1.0 s old\n"
        );
        assert!(
            render(
                "node1",
                &checkpoint,
                Pending {
                    count: 0,
                    oldest_ms: None
                },
                10_000
            )
            .contains("waiting to ship: nothing")
        );
    }
}
