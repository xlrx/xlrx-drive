//! Live notifications of journal changes (PLAN 5.2): the journal trigger's NOTIFY becomes a watch
//! channel holding the highest committed sequence number. Bursts (a scan writes hundreds of
//! entries) collapse into one wake-up per subscriber.

use std::time::Duration;

use sqlx::postgres::PgListener;
use tokio::sync::watch;

use crate::error::ApiResult;
use crate::state::AppState;

/// Missed notifications (connection lost and re-established) are caught up at least this often.
const CATCH_UP: Duration = Duration::from_secs(30);

/// The highest committed journal sequence, updated live. The listener starts with the first
/// subscriber.
pub async fn subscribe(st: &AppState) -> ApiResult<watch::Receiver<i64>> {
    let rx = st.live.get_or_try_init(|| start(st)).await?;
    Ok(rx.clone())
}

async fn max_seq(db: &sqlx::PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM journal")
        .fetch_one(db)
        .await
}

async fn start(st: &AppState) -> ApiResult<watch::Receiver<i64>> {
    let mut listener = PgListener::connect_with(&st.db).await?;
    listener.listen("xlrx_journal").await?;
    let (tx, rx) = watch::channel(max_seq(&st.db).await?);
    let db = st.db.clone();
    tokio::spawn(async move {
        let advance = |seq: i64| {
            tx.send_if_modified(|cur| {
                let newer = seq > *cur;
                if newer {
                    *cur = seq;
                }
                newer
            });
        };
        let mut tick = tokio::time::interval(CATCH_UP);
        loop {
            tokio::select! {
                n = listener.recv() => match n {
                    Ok(n) => {
                        if let Ok(seq) = n.payload().parse::<i64>() {
                            advance(seq);
                        }
                    }
                    // Shutting down: let go of the connection, or closing the pool waits for it.
                    Err(sqlx::Error::PoolClosed) => return,
                    Err(e) => {
                        tracing::warn!(error = %e, "Journal-Benachrichtigungen unterbrochen");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                },
                _ = tick.tick() => {
                    if let Ok(seq) = max_seq(&db).await {
                        advance(seq);
                    }
                }
            }
        }
    });
    Ok(rx)
}
