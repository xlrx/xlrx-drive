//! The bell (PLAN 8.4): the latest notifications and marking them read.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use super::files::{NodeInfo, folder_names};
use crate::auth::session::CurrentUser;
use crate::bell;
use crate::error::ApiResult;
use crate::files::access;
use crate::state::AppState;

#[derive(Serialize)]
pub struct Notification {
    pub id: i64,
    pub kind: String,
    pub actor: Option<String>,
    pub node: NodeInfo,
    pub folder: String,
    pub details: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub read: bool,
}

#[derive(Serialize)]
pub struct Bell {
    pub unread: usize,
    pub items: Vec<Notification>,
}

pub async fn list(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Json<Bell>> {
    let rows = bell::visible(&st.db, me.id).await?;
    let people: Vec<i64> = rows.iter().filter_map(|(r, _)| r.actor_user_id).collect();
    let names: HashMap<i64, String> =
        sqlx::query_as::<_, (i64, String)>("SELECT id, display_name FROM users WHERE id = ANY($1)")
            .bind(&people)
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    let scope = access::scope(&st.db, me.id).await?;
    let nodes: Vec<_> = rows.iter().map(|(_, n)| n.clone()).collect();
    let folders = folder_names(&st, &scope, &nodes).await?;
    let unread = rows.iter().filter(|(r, _)| r.read_at.is_none()).count();
    let items = rows
        .into_iter()
        .take(30)
        .map(|(r, n)| Notification {
            id: r.id,
            kind: r.kind,
            actor: r.actor_user_id.and_then(|a| names.get(&a).cloned()),
            folder: folders.get(&n.id).cloned().unwrap_or_default(),
            node: NodeInfo::from(&n),
            details: r.details,
            at: r.created_at,
            read: r.read_at.is_some(),
        })
        .collect();
    Ok(Json(Bell { unread, items }))
}

#[derive(Deserialize)]
pub struct ReadReq {
    /// Without: all.
    pub ids: Option<Vec<i64>>,
}

pub async fn read(
    State(st): State<AppState>,
    me: CurrentUser,
    Json(req): Json<ReadReq>,
) -> ApiResult<StatusCode> {
    bell::read(&st, me.id, req.ids.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}
