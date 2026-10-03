//! What a person opened (PLAN 8.1 `access_events`, private) and the suggestions on the start page
//! (PLAN 8.2): cards with a reason, scored from
//! - how often and how recently the person opened it (frecency, half-life 7 days),
//! - a weekly habit ("öffnest du meist montags"),
//! - files opened together with the one opened last,
//! - changes by others to files the person knows,
//! - items newly shared with them.
//!
//! Fixed weights for now; what was shown and opened is logged to tune them later.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::files::{NodeInfo, folder_names};
use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::access::{self, Role};
use crate::files::db::{NODE_COLS, NodeRow};
use crate::state::AppState;

/// Opening the same item again within this time counts once.
const DEDUPE_MINUTES: i32 = 10;
/// How far back a device may report an opening (the Mac reports what was opened locally).
const REPORT_DAYS: i64 = 30;

/// Records that a person opened or downloaded an item (deduplicated).
pub async fn record(
    st: &AppState,
    user_id: i64,
    node_id: i64,
    kind: &str,
    source: &str,
    at: Option<OffsetDateTime>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO access_events (user_id, node_id, kind, source, at)
         SELECT $1, $2, $3, $4, coalesce($5, now())
          WHERE NOT EXISTS (
                SELECT 1 FROM access_events
                 WHERE user_id = $1 AND node_id = $2 AND kind = $3
                   AND at BETWEEN coalesce($5, now()) - make_interval(mins => $6)
                              AND coalesce($5, now()) + make_interval(mins => $6))",
    )
    .bind(user_id)
    .bind(node_id)
    .bind(kind)
    .bind(source)
    .bind(at)
    .bind(DEDUPE_MINUTES)
    .execute(&st.db)
    .await?;
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct OpenedReq {
    /// `web` (default), `ios` or `mac`.
    pub source: Option<String>,
    /// When it was opened, for devices reporting later (at most 30 days back).
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub at: Option<OffsetDateTime>,
}

/// The person opened an item (the web app when showing a file; devices for local openings).
pub async fn opened(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    body: Option<Json<OpenedReq>>,
) -> ApiResult<StatusCode> {
    let req = body.map(|b| b.0).unwrap_or_default();
    let a = access::require(&st.db, me.id, id, Role::Viewer).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    let source = match req.source.as_deref().unwrap_or("web") {
        s @ ("web" | "ios" | "mac") => s,
        _ => return Err(ApiError::bad("Unbekannte Quelle.")),
    };
    if let Some(at) = req.at {
        let now = OffsetDateTime::now_utc();
        if at > now + time::Duration::minutes(5) || at < now - time::Duration::days(REPORT_DAYS) {
            return Err(ApiError::bad("Zeitpunkt außerhalb der letzten 30 Tage."));
        }
    }
    record(&st, me.id, id, "open", source, req.at).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Why something is suggested; the web app turns it into words.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// Opened often or lately.
    Opened {
        #[serde(with = "time::serde::rfc3339")]
        last: OffsetDateTime,
        count: i64,
    },
    /// Usually opened on this weekday (1 = Monday … 7 = Sunday).
    Weekly { weekday: i32, weeks: i64 },
    /// Opened together with the item opened last.
    Together { with: String },
    /// Changed by someone else (`who`: `None` when found on the NAS).
    Changed {
        who: Option<String>,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Newly shared with the person.
    Shared {
        by: Option<String>,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

impl Reason {
    fn name(&self) -> &'static str {
        match self {
            Reason::Opened { .. } => "opened",
            Reason::Weekly { .. } => "weekly",
            Reason::Together { .. } => "together",
            Reason::Changed { .. } => "changed",
            Reason::Shared { .. } => "shared",
        }
    }
}

#[derive(Serialize)]
pub struct Suggestion {
    #[serde(flatten)]
    pub node: NodeInfo,
    pub folder: String,
    pub reason: Reason,
    pub score: f64,
}

/// Exponential decay with the given half-life.
fn decay(at: OffsetDateTime, half_life_hours: f64) -> f64 {
    let hours = (OffsetDateTime::now_utc() - at).as_seconds_f64().max(0.0) / 3600.0;
    (-hours / half_life_hours * std::f64::consts::LN_2).exp()
}

/// Weighted parts of a score; the largest one is the reason shown.
#[derive(Default)]
struct Parts(Vec<(f64, Reason)>);

impl Parts {
    fn add(&mut self, score: f64, reason: Reason) {
        if score > 0.0 {
            self.0.push((score, reason));
        }
    }
    fn total(&self) -> f64 {
        self.0.iter().map(|(s, _)| s).sum()
    }
    fn reason(&self) -> Option<Reason> {
        self.0
            .iter()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, r)| r.clone())
    }
}

/// Weights (PLAN 8.2: fixed to start with).
const W_OPENED: f64 = 1.0;
const W_WEEKLY: f64 = 0.6;
const W_TOGETHER: f64 = 0.5;
const W_CHANGED: f64 = 2.0;
const W_SHARED: f64 = 2.5;
/// Cards shown.
const SHOWN: usize = 8;

pub async fn suggestions(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<Suggestion>>> {
    let ranked = ranked(&st, me.id).await?;
    let scope = access::scope(&st.db, me.id).await?;
    let rows: Vec<NodeRow> = ranked.iter().map(|r| r.2.clone()).collect();
    let folders = folder_names(&st, &scope, &rows).await?;
    // What was shown (at most once an hour per item and reason).
    for (score, reason, n) in &ranked {
        sqlx::query(
            "INSERT INTO suggestion_log (user_id, node_id, reason, score)
             SELECT $1, $2, $3, $4
              WHERE NOT EXISTS (
                    SELECT 1 FROM suggestion_log
                     WHERE user_id = $1 AND node_id = $2 AND reason = $3
                       AND shown_at > now() - interval '1 hour')",
        )
        .bind(me.id)
        .bind(n.id)
        .bind(reason.name())
        .bind(*score as f32)
        .execute(&st.db)
        .await?;
    }
    Ok(Json(
        ranked
            .into_iter()
            .map(|(score, reason, n)| Suggestion {
                folder: folders.get(&n.id).cloned().unwrap_or_default(),
                node: NodeInfo::from(&n),
                reason,
                score,
            })
            .collect(),
    ))
}

/// The person's suggestions, best first: score, reason, item (still there and visible).
pub async fn ranked(st: &AppState, user: i64) -> ApiResult<Vec<(f64, Reason, NodeRow)>> {
    let tz = &st.cfg.timezone;
    let mut parts: HashMap<i64, Parts> = HashMap::new();

    // Opened: often, lately, and on this weekday around this hour in earlier weeks.
    let opened: Vec<(i64, f64, i64, OffsetDateTime, i64, i32)> = sqlx::query_as(
        "SELECT node_id,
                sum(exp(-extract(epoch FROM now() - at) / 604800.0 * ln(2)))::float8,
                count(*),
                max(at),
                count(DISTINCT date_trunc('week', at AT TIME ZONE $2)) FILTER (
                    WHERE extract(isodow FROM at AT TIME ZONE $2) = extract(isodow FROM now() AT TIME ZONE $2)
                      AND abs(extract(hour FROM at AT TIME ZONE $2) - extract(hour FROM now() AT TIME ZONE $2)) <= 2
                      AND at < (date_trunc('day', now() AT TIME ZONE $2) AT TIME ZONE $2)),
                extract(isodow FROM now() AT TIME ZONE $2)::int
           FROM access_events
          WHERE user_id = $1 AND at > now() - interval '60 days'
          GROUP BY node_id",
    )
    .bind(user)
    .bind(tz)
    .fetch_all(&st.db)
    .await?;
    for (node, frecency, count, last, weeks, weekday) in &opened {
        let p = parts.entry(*node).or_default();
        p.add(
            W_OPENED * frecency,
            Reason::Opened {
                last: *last,
                count: *count,
            },
        );
        if *weeks >= 2 {
            p.add(
                W_WEEKLY * (*weeks).min(5) as f64,
                Reason::Weekly {
                    weekday: *weekday,
                    weeks: *weeks,
                },
            );
        }
    }
    let known: Vec<i64> = opened.iter().map(|o| o.0).collect();

    // Opened together with the item opened last (today).
    let last: Option<(i64, String)> = sqlx::query_as(
        "SELECT e.node_id, n.name FROM access_events e JOIN nodes n ON n.id = e.node_id
          WHERE e.user_id = $1 AND e.at > now() - interval '1 day' AND n.deleted_at IS NULL
          ORDER BY e.at DESC LIMIT 1",
    )
    .bind(user)
    .fetch_optional(&st.db)
    .await?;
    if let Some((anchor, anchor_name)) = &last {
        let together: Vec<(i64, i64)> = sqlx::query_as(
            "WITH a AS (
                SELECT at FROM access_events
                 WHERE user_id = $1 AND node_id = $2 AND at > now() - interval '60 days')
             SELECT e.node_id, count(DISTINCT a.at)
               FROM access_events e
               JOIN a ON e.at BETWEEN a.at - interval '30 minutes' AND a.at + interval '30 minutes'
              WHERE e.user_id = $1 AND e.node_id <> $2
              GROUP BY e.node_id
             HAVING count(DISTINCT a.at) >= 2",
        )
        .bind(user)
        .bind(anchor)
        .fetch_all(&st.db)
        .await?;
        for (node, n) in together {
            parts.entry(node).or_default().add(
                W_TOGETHER * n.min(4) as f64,
                Reason::Together {
                    with: anchor_name.clone(),
                },
            );
        }
    }

    // Changed by others (or on the NAS) lately, since the person last opened it.
    let changed: Vec<(i64, OffsetDateTime, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT ON (j.node_id) j.node_id, j.at, j.actor_user_id
           FROM journal j
          WHERE j.node_id = ANY($2) AND j.at > now() - interval '7 days'
            AND j.op IN ('create', 'update')
            AND (j.source = 'scan' OR (j.actor_user_id IS NOT NULL AND j.actor_user_id <> $1))
            -- Only what changed since the person last opened it.
            AND j.at > (SELECT max(e.at) FROM access_events e
                         WHERE e.user_id = $1 AND e.node_id = j.node_id)
          ORDER BY j.node_id, j.at DESC",
    )
    .bind(user)
    .bind(&known)
    .fetch_all(&st.db)
    .await?;

    // Newly shared with the person or one of their groups.
    let shared: Vec<(i64, OffsetDateTime, Option<i64>)> = sqlx::query_as(
        "SELECT DISTINCT ON (s.node_id) s.node_id, s.created_at, s.created_by
           FROM shares s
          WHERE s.created_at > now() - interval '14 days'
            AND (s.expires_at IS NULL OR s.expires_at > now())
            AND (s.user_id = $1 OR s.group_id IN (SELECT group_id FROM group_members WHERE user_id = $1))
            AND s.created_by IS DISTINCT FROM $1
          ORDER BY s.node_id, s.created_at DESC",
    )
    .bind(user)
    .fetch_all(&st.db)
    .await?;

    let mut people: Vec<i64> = changed.iter().filter_map(|c| c.2).collect();
    people.extend(shared.iter().filter_map(|s| s.2));
    let names: HashMap<i64, String> =
        sqlx::query_as::<_, (i64, String)>("SELECT id, display_name FROM users WHERE id = ANY($1)")
            .bind(&people)
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    for (node, at, who) in changed {
        parts.entry(node).or_default().add(
            W_CHANGED * decay(at, 48.0),
            Reason::Changed {
                who: who.and_then(|w| names.get(&w).cloned()),
                at,
            },
        );
    }
    for (node, at, by) in shared {
        parts.entry(node).or_default().add(
            W_SHARED * decay(at, 72.0),
            Reason::Shared {
                by: by.and_then(|b| names.get(&b).cloned()),
                at,
            },
        );
    }

    // Only what is still there and the person may still see; files, and folders shared with them.
    let ids: Vec<i64> = parts.keys().copied().collect();
    let nodes: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE id = ANY($1) AND deleted_at IS NULL"
    )))
    .bind(&ids)
    .fetch_all(&st.db)
    .await?;
    let roles = access::roles(&st.db, user, &nodes).await?;
    let mut ranked: Vec<(f64, Reason, NodeRow)> = nodes
        .into_iter()
        .filter(|n| roles.contains_key(&n.id))
        .filter_map(|n| {
            let p = parts.get(&n.id)?;
            let reason = p.reason()?;
            if n.is_dir() && !matches!(reason, Reason::Shared { .. }) {
                return None;
            }
            Some((p.total(), reason, n))
        })
        .filter(|(score, _, _)| *score >= 0.05)
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.2.id.cmp(&b.2.id)));
    ranked.truncate(SHOWN);
    Ok(ranked)
}

#[derive(Deserialize)]
pub struct SuggestionOpened {
    pub node_id: i64,
}

/// A suggestion was opened (for tuning the weights).
pub async fn suggestion_opened(
    State(st): State<AppState>,
    me: CurrentUser,
    Json(req): Json<SuggestionOpened>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "UPDATE suggestion_log SET opened_at = now()
          WHERE id = (SELECT id FROM suggestion_log WHERE user_id = $1 AND node_id = $2
                       ORDER BY shown_at DESC LIMIT 1)",
    )
    .bind(me.id)
    .bind(req.node_id)
    .execute(&st.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
