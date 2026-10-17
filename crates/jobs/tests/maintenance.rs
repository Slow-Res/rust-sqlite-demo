#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::maintenance::LEADER_LOCK;
use library_jobs::store::maintenance::{prune, reap};
use sqlx::PgPool;

async fn expire(pool: &PgPool, id: i64) {
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn row(pool: &PgPool, id: i64) -> (String, i32, String) {
    sqlx::query_as("SELECT state, crashes, errors->-1->>'cause' FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn reaper_requeues_expired_leases_with_the_right_cause() {
    with_db(|pool| async move {
        let live = register_process(&pool, "live").await;
        let a = insert_running(&pool, live, "k", 60, 900).await;
        let b = insert_running(&pool, -7, "k", 60, 900).await; // holder has no registry row
        expire(&pool, a).await;
        expire(&pool, b).await;
        let reaped = reap(&pool).await.unwrap();
        assert_eq!(reaped.len(), 2);
        assert_eq!(
            row(&pool, a).await,
            ("available".into(), 1, "lease_expired".into())
        );
        assert_eq!(
            row(&pool, b).await,
            ("available".into(), 1, "process_lost".into())
        );
        assert!(
            reap(&pool).await.unwrap().is_empty(),
            "nothing left to reap"
        );
    })
    .await;
}

#[tokio::test]
async fn poison_jobs_fail_after_max_crashes_and_attempts_are_respected() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = insert_running(&pool, me, "k", 60, 900).await; // max_crashes 2
        expire(&pool, id).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, id).await.0, "available");
        force_running(&pool, id, me).await;
        expire(&pool, id).await;
        reap(&pool).await.unwrap();
        assert_eq!(
            row(&pool, id).await,
            ("failed".into(), 2, "lease_expired".into())
        );

        let last = insert_running(&pool, me, "k", 60, 900).await;
        sqlx::query("UPDATE jobs SET max_attempts = 1, max_crashes = 5 WHERE id = $1")
            .bind(last)
            .execute(&pool)
            .await
            .unwrap();
        expire(&pool, last).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, last).await.0, "failed", "no attempts left");
    })
    .await;
}

#[tokio::test]
async fn pruner_removes_silent_processes_and_frees_their_jobs() {
    with_db(|pool| async move {
        let dead = register_process(&pool, "dead").await;
        let alive = register_process(&pool, "alive").await;
        let j = insert_running(&pool, dead, "k", 60, 900).await;
        let keep = insert_running(&pool, alive, "k", 60, 900).await;
        sqlx::query(
            "UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(dead)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            prune(&pool, Duration::from_secs(180)).await.unwrap(),
            (1, 1)
        );
        reap(&pool).await.unwrap();
        assert_eq!(
            row(&pool, j).await,
            ("available".into(), 1, "process_lost".into())
        );
        assert_eq!(job_state(&pool, keep).await, "running");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_process_job_is_recovered_by_a_live_worker() {
    with_db(|pool| async move {
        // What `kill -9` leaves behind: a registry row that stops beating and a running job.
        let dead = register_process(&pool, "dead").await;
        let id = enqueue(&pool, &SleepOnce { ms: 60_000 }).await;
        force_running(&pool, id, dead).await;
        sqlx::query("UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour'")
            .execute(&pool)
            .await
            .unwrap();
        let w = start(&pool, fast_config("default:2"));
        eventually(Duration::from_secs(10), "re-run and succeeded", || async {
            job_state(&pool, id).await == "succeeded"
        })
        .await;
        let (crashes, cause): (i32, String) =
            sqlx::query_as("SELECT crashes, errors->0->>'cause' FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((crashes, cause.as_str()), (1, "process_lost"));
        w.shutdown().await;
    })
    .await;
}

async fn leaders(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(LEADER_LOCK)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The backend pid holding the leader lock in this database, if any.
async fn leader_pid(pool: &PgPool) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(LEADER_LOCK)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_one_leader_and_failover_when_its_connection_dies() {
    with_db(|pool| async move {
        let a = start(&pool, fast_config("default:1"));
        let b = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "a leader", || async {
            leaders(&pool).await == 1
        })
        .await;
        for _ in 0..20 {
            assert!(leaders(&pool).await <= 1);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let old_pid = leader_pid(&pool).await.expect("a leader holds the lock");
        sqlx::query("SELECT pg_terminate_backend($1)")
            .bind(old_pid)
            .execute(&pool)
            .await
            .unwrap();
        // A different backend must take the lock over.
        eventually(
            Duration::from_secs(5),
            "a different backend holds the lock",
            || async { leader_pid(&pool).await.is_some_and(|pid| pid != old_pid) },
        )
        .await;
        // The old leader's loop notices its dead connection within one `leader_retry`
        // (200 ms here); give it two, so only the new backend's leader can reap below.
        tokio::time::sleep(Duration::from_millis(400)).await;
        // A lease that expires now can only be reaped by the new leader.
        let me = register_process(&pool, "victim").await;
        let id = insert_running(&pool, me, "k", 60, 900).await;
        expire(&pool, id).await;
        eventually(Duration::from_secs(5), "reaped after failover", || async {
            job_state(&pool, id).await == "available"
        })
        .await;
        assert_eq!(leaders(&pool).await, 1);
        a.shutdown().await;
        b.shutdown().await;
    })
    .await;
}

/// The `jobs_leader` gauge as a whole line: its HELP text also contains "jobs_leader 1".
fn is_leader(m: &library_jobs::metrics::Metrics) -> bool {
    m.encode().lines().any(|l| l == "jobs_leader 1")
}

/// A TCP relay to Postgres that can start swallowing traffic without closing
/// anything: what a network partition looks like to the client (no error, no data).
struct Blackhole {
    addr: std::net::SocketAddr,
    swallow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

async fn blackhole_proxy(upstream: &str) -> Blackhole {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let swallow = std::sync::Arc::new(AtomicBool::new(false));
    let (upstream, flag) = (upstream.to_owned(), swallow.clone());
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
                continue;
            };
            let (mut cr, mut cw) = client.into_split();
            let (mut sr, mut sw) = server.into_split();
            let (f1, f2) = (flag.clone(), flag.clone());
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                while let Ok(n) = cr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if !f1.load(Ordering::Relaxed) && sw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                while let Ok(n) = sr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if !f2.load(Ordering::Relaxed) && cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Blackhole { addr, swallow }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_open_leader_connection_is_noticed_and_leadership_given_up() {
    with_db_url(|_pool, url| async move {
        let opts: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        let proxy = blackhole_proxy(&format!("{}:{}", opts.get_host(), opts.get_port())).await;
        let via_proxy = sqlx::postgres::PgPoolOptions::new()
            .max_connections(10)
            .connect_with(opts.clone().host("127.0.0.1").port(proxy.addr.port()))
            .await
            .unwrap();
        let w = start(&via_proxy, fast_config("default:1"));
        eventually(Duration::from_secs(5), "leader", || async {
            is_leader(&w.metrics)
        })
        .await;
        // The connection stays open but nothing gets through any more.
        proxy
            .swallow
            .store(true, std::sync::atomic::Ordering::Relaxed);
        eventually(
            Duration::from_secs(5),
            "leadership given up on a silent connection",
            || async { !is_leader(&w.metrics) },
        )
        .await;
        // The worker's other connections are dead too, so its own shutdown cannot finish: abandon it.
        w.handle.abort();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_leader_exports_queue_gauges() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let opts = library_jobs::EnqueueOptions {
            delay: Some(Duration::from_secs(3600)),
            ..Default::default()
        };
        for n in 0..2 {
            library_jobs::Enqueuer::new(false)
                .enqueue_with(&mut conn, &Noop { n }, opts.clone())
                .await
                .unwrap();
        }
        let w = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "gauges", || async {
            w.metrics
                .encode()
                .contains(r#"jobs_scheduled{queue="default"} 2"#)
        })
        .await;
        assert!(is_leader(&w.metrics));
        w.shutdown().await;
    })
    .await;
}

/// Right after an outage every live process looks silent, so the leader prunes only
/// once the database has been healthy for `prune_after` (spec §11.5). A failing
/// retention statement (its table is renamed away) keeps the leader "unhealthy".
#[tokio::test(flavor = "multi_thread")]
async fn the_leader_does_not_prune_while_its_statements_keep_failing() {
    with_db(|pool| async move {
        sqlx::query("ALTER TABLE job_recurring_runs RENAME TO job_recurring_runs_away")
            .execute(&pool)
            .await
            .unwrap();
        let ghost = register_process(&pool, "ghost").await;
        sqlx::query(
            "UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(ghost)
        .execute(&pool)
        .await
        .unwrap();
        // fast_config: prune_after 1.5 s, retention every 0.5 s, prune every 0.2 s.
        let w = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "leader", || async {
            is_leader(&w.metrics)
        })
        .await;
        // Twice prune_after of failing statements: the silent row must survive.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let ghosts = count(
            &pool,
            "SELECT count(*) FROM job_processes WHERE name = 'ghost'",
        )
        .await;
        assert_eq!(
            ghosts, 1,
            "pruned while the leader's statements were failing"
        );

        sqlx::query("ALTER TABLE job_recurring_runs_away RENAME TO job_recurring_runs")
            .execute(&pool)
            .await
            .unwrap();
        eventually(
            Duration::from_secs(10),
            "pruned after a healthy stretch",
            || async {
                count(
                    &pool,
                    "SELECT count(*) FROM job_processes WHERE name = 'ghost'",
                )
                .await
                    == 0
            },
        )
        .await;
        w.shutdown().await;
    })
    .await;
}
