//! What the administration shows of the AI search (PLAN 6.6, 7.3): providers and models, whether
//! the cloud runs, the month's budget, progress, costs per month and an estimate of what the
//! rest will cost, from what was spent so far (after a trial run: its real costs per content).

use serde::Serialize;

use super::{Space, cloud_paused, month_spent};
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Serialize, Debug)]
pub struct Status {
    pub cloud: Option<CloudStatus>,
    pub local: Option<LocalStatus>,
    pub progress: Progress,
    /// Last twelve months, newest first.
    pub usage: Vec<Usage>,
    pub estimate: Option<Estimate>,
}

#[derive(Serialize, Debug)]
pub struct CloudStatus {
    /// Host of the provider (e.g. `api.scaleway.ai`).
    pub provider: String,
    pub embed_model: String,
    pub vision_model: Option<String>,
    /// Started by an administrator.
    pub on: bool,
    /// Contents left in a trial run.
    pub trial_left: Option<i32>,
    /// Why it does not run now, if it does not.
    pub paused: Option<&'static str>,
    pub budget: f64,
    /// Set in the administration (otherwise the configured value applies).
    pub budget_set: bool,
    pub spent_month: f64,
    pub error: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct LocalStatus {
    pub embed_model: String,
    pub clip_model: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Debug, Default)]
pub struct Progress {
    /// Contents done: texts and pictures in the cloud, texts and pictures in the home network.
    pub cloud_texts: i64,
    pub cloud_pictures: i64,
    pub local_texts: i64,
    pub local_pictures: i64,
    /// Waiting to be done, per side.
    pub queued_cloud: i64,
    pub queued_local: i64,
    /// Given up (the provider refused, or it failed too often).
    pub failed: i64,
    /// Done within the last hour, per side (for how long the rest takes).
    pub last_hour_cloud: i64,
    pub last_hour_local: i64,
}

#[derive(Serialize, Debug, sqlx::FromRow)]
pub struct Usage {
    pub month: time::Date,
    pub kind: String,
    pub calls: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost: f64,
}

#[derive(Serialize, Debug)]
pub struct Estimate {
    /// Euros per content so far: a text, a picture.
    pub per_text: f64,
    pub per_picture: f64,
    /// What the queued contents will cost.
    pub remaining: f64,
}

pub async fn status(st: &AppState) -> ApiResult<Status> {
    let ai = &st.ai;
    let db = &st.db;
    let (on, trial_left, budget): (bool, Option<i32>, Option<f64>) =
        sqlx::query_as("SELECT cloud_on, trial_left, budget FROM ai_state")
            .fetch_one(db)
            .await?;
    let cloud = match &st.cfg.ai.cloud {
        None => None,
        Some(c) => Some(CloudStatus {
            provider: c.url.host_str().unwrap_or_default().to_owned(),
            embed_model: c.embed_model.clone(),
            vision_model: c.vision_model.clone(),
            on,
            trial_left,
            paused: cloud_paused(st, 0.0).await?,
            budget: budget.unwrap_or(st.cfg.ai.budget),
            budget_set: budget.is_some(),
            spent_month: month_spent(st).await?,
            error: ai.error(super::Side::Cloud),
        }),
    };
    let local = st.cfg.ai.local.as_ref().map(|l| LocalStatus {
        embed_model: l.embed_model.clone(),
        clip_model: l.clip_model.clone(),
        error: ai.error(super::Side::Local),
    });
    let mut p = Progress::default();
    let done: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT space, task, count(*), count(*) FILTER (WHERE at > now() - interval '1 hour')
           FROM ai_done GROUP BY space, task",
    )
    .fetch_all(db)
    .await?;
    for (space, task, n, hour) in done {
        match (space.as_str(), task.as_str()) {
            ("cloud", "text") => p.cloud_texts = n,
            ("cloud", "image") => p.cloud_pictures = n,
            ("local", "text") => p.local_texts = n,
            ("clip", "image") => p.local_pictures = n,
            _ => {}
        }
        if space == Space::Cloud.as_str() {
            p.last_hour_cloud += hour;
        } else {
            p.last_hour_local += hour;
        }
    }
    let jobs: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT kind, state, count(*) FROM jobs WHERE kind IN ('ai_cloud', 'ai_local')
          GROUP BY kind, state",
    )
    .fetch_all(db)
    .await?;
    for (kind, state, n) in jobs {
        match (kind.as_str(), state.as_str()) {
            (_, "failed") => p.failed += n,
            ("ai_cloud", _) => p.queued_cloud += n,
            (_, _) => p.queued_local += n,
        }
    }
    let usage: Vec<Usage> = sqlx::query_as(
        "SELECT month, kind, calls, tokens_in, tokens_out, cost FROM ai_usage
          WHERE month > (now() - interval '12 months')::date ORDER BY month DESC, kind",
    )
    .fetch_all(db)
    .await?;
    let estimate = estimate(st, &usage, &p).await?;
    Ok(Status {
        cloud,
        local,
        progress: p,
        usage,
        estimate,
    })
}

/// Euros per text and per picture so far, and what the queued ones will cost.
async fn estimate(st: &AppState, usage: &[Usage], p: &Progress) -> ApiResult<Option<Estimate>> {
    let cost = |kind: &str| -> f64 {
        usage
            .iter()
            .filter(|u| u.kind == kind)
            .map(|u| u.cost)
            .sum()
    };
    let (embed, vision) = (cost("embed"), cost("vision"));
    let done = p.cloud_texts + p.cloud_pictures;
    if done == 0 || embed + vision <= 0.0 {
        return Ok(None);
    }
    // Each picture also had its description embedded.
    let per_text = embed / done as f64;
    let per_picture = if p.cloud_pictures > 0 {
        vision / p.cloud_pictures as f64 + per_text
    } else {
        per_text
    };
    let (texts, pictures): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE key LIKE 'text:%'), count(*) FILTER (WHERE key LIKE 'image:%')
           FROM jobs WHERE kind = 'ai_cloud' AND state <> 'failed'",
    )
    .fetch_one(&st.db)
    .await?;
    Ok(Some(Estimate {
        per_text,
        per_picture,
        remaining: texts as f64 * per_text + pictures as f64 * per_picture,
    }))
}

/// What an administrator can do.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Let the cloud run.
    Start,
    /// Stop it (also a trial run).
    Stop,
    /// Handle this many contents, then stop again.
    Trial,
}

/// Starts or stops the cloud, or starts a trial run of `trial` contents.
pub async fn act(st: &AppState, action: Action, trial: Option<i32>) -> ApiResult<()> {
    match action {
        Action::Start => {
            sqlx::query("UPDATE ai_state SET cloud_on = true, trial_left = NULL")
                .execute(&st.db)
                .await?;
        }
        Action::Stop => {
            sqlx::query("UPDATE ai_state SET cloud_on = false, trial_left = NULL")
                .execute(&st.db)
                .await?;
        }
        Action::Trial => {
            sqlx::query("UPDATE ai_state SET cloud_on = false, trial_left = $1")
                .bind(trial.unwrap_or(500).clamp(1, 10_000))
                .execute(&st.db)
                .await?;
        }
    }
    st.ai.work.notify_waiters();
    Ok(())
}

/// Sets the month's budget (`None`: the configured one).
pub async fn set_budget(st: &AppState, budget: Option<f64>) -> ApiResult<()> {
    sqlx::query("UPDATE ai_state SET budget = $1")
        .bind(budget)
        .execute(&st.db)
        .await?;
    st.ai.work.notify_waiters();
    Ok(())
}
