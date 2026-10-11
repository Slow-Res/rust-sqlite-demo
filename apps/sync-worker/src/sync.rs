//! One sync cycle (spec §6.3–6.5): check the checkpoint, then ship the next
//! batch, or copy the whole node when the checkpoint no longer matches.

use sqlx::SqlitePool;

use crate::changes::{self, Checkpoint, Read};
use crate::mirror::Mirror;

/// What one cycle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing new.
    Idle,
    /// A batch was applied. `full` means it hit the batch size, so more
    /// changes are probably waiting.
    Shipped {
        changes: usize,
        rows: usize,
        full: bool,
    },
    /// The whole node was copied. `resync` is false for the very first copy
    /// (bootstrap) and true when a restore was detected.
    Copied {
        resync: bool,
        epoch: i32,
        archived: u64,
        copied: u64,
    },
}

/// Ships one node's changes. Create one per Postgres session: it reads the
/// checkpoint from Postgres on its first cycle, then keeps it in memory,
/// because it is the only writer of that checkpoint (spec §6.6).
pub struct Syncer {
    sqlite: SqlitePool,
    mirror: Mirror,
    batch_size: usize,
    checkpoint: Option<Checkpoint>,
}

impl Syncer {
    pub fn new(sqlite: SqlitePool, mirror: Mirror, batch_size: usize) -> Self {
        Self {
            sqlite,
            mirror,
            batch_size,
            checkpoint: None,
        }
    }

    /// The checkpoint after the last successful cycle.
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    /// Hand back the Postgres side, e.g. to close it.
    pub fn into_mirror(self) -> Mirror {
        self.mirror
    }

    pub async fn cycle(&mut self) -> anyhow::Result<Outcome> {
        let checkpoint = match self.checkpoint.clone() {
            Some(checkpoint) => checkpoint,
            None => match self.mirror.load_checkpoint().await? {
                Some(checkpoint) => checkpoint,
                None => return self.full_copy(None).await,
            },
        };
        match changes::read_batch(&self.sqlite, &checkpoint, self.batch_size).await? {
            Read::Invalid => self.full_copy(Some(checkpoint)).await,
            Read::Empty => {
                self.checkpoint = Some(checkpoint);
                Ok(Outcome::Idle)
            }
            Read::Batch(batch) => {
                self.mirror.apply_batch(&batch, checkpoint.epoch).await?;
                self.checkpoint = Some(Checkpoint {
                    epoch: checkpoint.epoch,
                    last_seq: batch.last_seq,
                    last_token: Some(batch.last_token.clone()),
                });
                // Postgres has committed, so the outcome is decided. If
                // pruning fails, the next successful cycle prunes these rows
                // too (it removes everything below its own last_seq).
                self.prune_after_commit(batch.last_seq).await;
                Ok(Outcome::Shipped {
                    changes: batch.changes,
                    rows: batch.rows(),
                    full: batch.changes >= self.batch_size,
                })
            }
        }
    }

    /// Prune after Postgres has committed. A failure is only logged: the
    /// outcome is already decided, and the next successful cycle prunes the
    /// rows this one missed.
    async fn prune_after_commit(&self, below_seq: i64) {
        if let Err(err) = changes::prune(&self.sqlite, below_seq).await {
            tracing::warn!(
                node = self.mirror.node_id(),
                error = format!("{err:#}"),
                "pruning shipped changes failed; the next batch prunes them"
            );
        }
    }

    /// Copy every row, archiving what Postgres had (spec §6.5). `old` is the
    /// checkpoint that failed the check, or `None` on the very first copy.
    async fn full_copy(&mut self, old: Option<Checkpoint>) -> anyhow::Result<Outcome> {
        let snapshot = changes::read_snapshot(&self.sqlite).await?;
        let old_epoch = old.as_ref().map_or(0, |c| c.epoch);
        let report = self.mirror.replace_all(&snapshot, old_epoch).await?;
        self.checkpoint = Some(Checkpoint {
            epoch: report.epoch,
            last_seq: snapshot.seq,
            last_token: snapshot.token.clone(),
        });
        let resync = old.is_some();
        if resync {
            tracing::warn!(
                node = self.mirror.node_id(),
                epoch = report.epoch,
                archived = report.archived,
                copied = report.copied,
                "the checkpoint did not match this database (restored from an older backup?); \
                 copied the whole node and archived the rows Postgres had"
            );
        } else {
            tracing::info!(
                node = self.mirror.node_id(),
                copied = report.copied,
                archived = report.archived,
                "first sync for this node: copied every row"
            );
        }
        self.prune_after_commit(snapshot.seq).await;
        Ok(Outcome::Copied {
            resync,
            epoch: report.epoch,
            archived: report.archived,
            copied: report.copied,
        })
    }
}
