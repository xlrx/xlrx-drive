//! Sharing (PLAN 9.1): who has access to an item, sharing it with people and groups, and what
//! others shared with me.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::files::NodeInfo;
use crate::audit;
use crate::auth::session::{ClientInfo, CurrentUser};
use crate::error::{ApiError, ApiResult};
use crate::files::access::{self, Role};
use crate::files::db::{NODE_COLS, NodeRow};
use crate::state::AppState;

/// A person or a group.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase", tag = "type", content = "id")]
pub enum Principal {
    User(i64),
    Group(i64),
}

impl Principal {
    fn from_columns(user: Option<i64>, group: Option<i64>) -> Option<Self> {
        match (user, group) {
            (Some(u), None) => Some(Principal::User(u)),
            (None, Some(g)) => Some(Principal::Group(g)),
            _ => None,
        }
    }

    fn user(self) -> Option<i64> {
        match self {
            Principal::User(u) => Some(u),
            Principal::Group(_) => None,
        }
    }

    fn group(self) -> Option<i64> {
        match self {
            Principal::Group(g) => Some(g),
            Principal::User(_) => None,
        }
    }
}

#[derive(Serialize)]
pub struct Person {
    pub id: i64,
    pub name: String,
    pub username: String,
}

#[derive(Serialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub members: i64,
}

#[derive(Serialize)]
pub struct People {
    pub users: Vec<Person>,
    pub groups: Vec<Group>,
}

/// The people and groups something can be shared with (everyone else on this server).
pub async fn people(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Json<People>> {
    let users: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, display_name, username FROM users
          WHERE disabled_at IS NULL AND id <> $1 ORDER BY lower(display_name), username",
    )
    .bind(me.id)
    .fetch_all(&st.db)
    .await?;
    let groups: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT g.id, g.name, count(m.user_id) FROM groups g
           LEFT JOIN group_members m ON m.group_id = g.id
          GROUP BY g.id ORDER BY lower(g.name)",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(Json(People {
        users: users
            .into_iter()
            .map(|(id, name, username)| Person { id, name, username })
            .collect(),
        groups: groups
            .into_iter()
            .map(|(id, name, members)| Group { id, name, members })
            .collect(),
    }))
}

#[derive(Serialize)]
pub struct Named {
    #[serde(flatten)]
    pub principal: Principal,
    pub name: String,
}

#[derive(Serialize)]
pub struct ShareInfo {
    pub id: i64,
    pub to: Named,
    pub role: Role,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    pub expired: bool,
    /// The item the share is on; for shares on a folder above, this one inherits it.
    pub node_id: i64,
    pub node_name: String,
    pub inherited: bool,
    pub created_by: Option<String>,
}

#[derive(Serialize)]
pub struct AccessInfo {
    /// My role here.
    pub role: Role,
    /// May I share this (and change who it is shared with)?
    pub can_share: bool,
    /// Owner of the home it is in (none for shared roots).
    pub owner: Option<String>,
    /// The shared root it is in, with its members.
    pub space: Option<String>,
    pub members: Vec<MemberInfo>,
    pub shares: Vec<ShareInfo>,
}

#[derive(Serialize)]
pub struct MemberInfo {
    pub to: Named,
    pub role: Role,
}

#[derive(sqlx::FromRow)]
struct ShareRow {
    id: i64,
    node_id: i64,
    user_id: Option<i64>,
    group_id: Option<i64>,
    role: String,
    expires_at: Option<OffsetDateTime>,
    created_by: Option<i64>,
}

const SHARE_COLS: &str = "id, node_id, user_id, group_id, role, expires_at, created_by";

async fn name_of(st: &AppState, p: Principal) -> ApiResult<String> {
    let name: Option<String> = match p {
        Principal::User(u) => {
            sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
                .bind(u)
                .fetch_optional(&st.db)
                .await?
        }
        Principal::Group(g) => {
            sqlx::query_scalar("SELECT name FROM groups WHERE id = $1")
                .bind(g)
                .fetch_optional(&st.db)
                .await?
        }
    };
    Ok(name.unwrap_or_else(|| "–".into()))
}

async fn user_name(st: &AppState, id: Option<i64>) -> ApiResult<Option<String>> {
    let Some(id) = id else { return Ok(None) };
    Ok(
        sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await?,
    )
}

async fn share_info(
    st: &AppState,
    r: ShareRow,
    here: i64,
    node_name: String,
) -> ApiResult<ShareInfo> {
    let principal = Principal::from_columns(r.user_id, r.group_id)
        .ok_or_else(|| ApiError::Internal("Freigabe ohne Empfänger".into()))?;
    Ok(ShareInfo {
        id: r.id,
        to: Named {
            principal,
            name: name_of(st, principal).await?,
        },
        role: Role::parse(&r.role).unwrap_or(Role::Viewer),
        expired: r.expires_at.is_some_and(|e| e <= OffsetDateTime::now_utc()),
        expires_at: r.expires_at,
        inherited: r.node_id != here,
        node_id: r.node_id,
        node_name,
        created_by: user_name(st, r.created_by).await?,
    })
}

/// Who has access to an item: owner or shared root with its members, and the shares on it and on
/// the folders above it that the person can see.
pub async fn node_access(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<AccessInfo>> {
    let a = access::require(&st.db, me.id, id, Role::Viewer).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    let scope = access::scope(&st.db, me.id).await?;
    // Shares above the highest folder the person can see stay hidden (they would name it).
    let chain = access::chains(&st.db, &scope, &[a.node.id])
        .await?
        .remove(&a.node.id)
        .unwrap_or_default();
    let ids: Vec<i64> = chain.iter().map(|(id, _)| *id).collect();
    let rows: Vec<ShareRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SHARE_COLS} FROM shares WHERE node_id = ANY($1) ORDER BY created_at, id"
    )))
    .bind(&ids)
    .fetch_all(&st.db)
    .await?;
    let mut shares = Vec::with_capacity(rows.len());
    for r in rows {
        let name = chain
            .iter()
            .find(|(id, _)| *id == r.node_id)
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        shares.push(share_info(&st, r, a.node.id, name).await?);
    }
    let (owner, space, members) = if a.root.kind == "home" {
        (user_name(&st, a.root.owner_user_id).await?, None, vec![])
    } else {
        let rows: Vec<(Option<i64>, Option<i64>, String)> =
            sqlx::query_as("SELECT user_id, group_id, role FROM root_members WHERE root_id = $1")
                .bind(a.root.id)
                .fetch_all(&st.db)
                .await?;
        let mut members = Vec::new();
        for (u, g, role) in rows {
            if let Some(p) = Principal::from_columns(u, g) {
                members.push(MemberInfo {
                    to: Named {
                        principal: p,
                        name: name_of(&st, p).await?,
                    },
                    role: Role::parse(&role).unwrap_or(Role::Viewer),
                });
            }
        }
        (None, Some(a.root.name.clone()), members)
    };
    Ok(Json(AccessInfo {
        role: a.role,
        can_share: a.role >= Role::Manager,
        owner,
        space,
        members,
        shares,
    }))
}

#[derive(Deserialize)]
pub struct ShareReq {
    #[serde(flatten)]
    pub to: Principal,
    pub role: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

fn grantable(role: &str, mine: Role) -> ApiResult<Role> {
    let r = Role::parse(role).ok_or_else(|| ApiError::bad("Unbekannte Rolle."))?;
    if r > mine {
        return Err(ApiError::forbidden(
            "Du kannst nicht mehr Rechte weitergeben, als du selbst hast.",
        ));
    }
    Ok(r)
}

fn future(expires_at: Option<OffsetDateTime>) -> ApiResult<()> {
    if expires_at.is_some_and(|e| e <= OffsetDateTime::now_utc()) {
        return Err(ApiError::bad("Das Ablaufdatum liegt in der Vergangenheit."));
    }
    Ok(())
}

/// Shares an item with a person or group (or changes the role if it is shared with them already).
pub async fn create(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<ShareReq>,
) -> ApiResult<(StatusCode, Json<ShareInfo>)> {
    let a = access::require(&st.db, me.id, id, Role::Manager).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    if a.node.parent_id.is_none() {
        return Err(ApiError::bad(
            "Eine ganze Ablage wird nicht geteilt – nur Ordner und Dateien darin.",
        ));
    }
    let role = grantable(&req.role, a.role)?;
    future(req.expires_at)?;
    match req.to {
        Principal::User(u) => {
            if u == me.id {
                return Err(ApiError::bad("Mit dir selbst musst du nichts teilen."));
            }
            if a.root.kind == "home" && a.root.owner_user_id == Some(u) {
                return Err(ApiError::bad("Das gehört dieser Person bereits."));
            }
            let ok: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND disabled_at IS NULL)",
            )
            .bind(u)
            .fetch_one(&st.db)
            .await?;
            if !ok {
                return Err(ApiError::bad("Unbekannte Person."));
            }
        }
        Principal::Group(g) => {
            let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM groups WHERE id = $1)")
                .bind(g)
                .fetch_one(&st.db)
                .await?;
            if !ok {
                return Err(ApiError::bad("Unbekannte Gruppe."));
            }
        }
    }
    let row: ShareRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO shares (node_id, user_id, group_id, role, expires_at, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (node_id, user_id, group_id)
            DO UPDATE SET role = EXCLUDED.role, expires_at = EXCLUDED.expires_at
         RETURNING {SHARE_COLS}"
    )))
    .bind(a.node.id)
    .bind(req.to.user())
    .bind(req.to.group())
    .bind(role.as_str())
    .bind(req.expires_at)
    .bind(me.id)
    .fetch_one(&st.db)
    .await?;
    audit::log(
        &st.db,
        Some(me.id),
        req.to.user(),
        "share_created",
        Some(&client.ip),
        json!({ "node": a.node.id, "to": req.to, "role": role, "expires_at": req.expires_at }),
    )
    .await?;
    let info = share_info(&st, row, a.node.id, a.node.name.clone()).await?;
    crate::events::record(
        &st.db,
        Some(me.id),
        a.node.id,
        "shared",
        json!({ "to": info.to.name, "role": role }),
    )
    .await?;
    crate::bell::shared(
        &st,
        me.id,
        a.node.id,
        req.to.user(),
        req.to.group(),
        json!({ "role": role, "group": matches!(req.to, Principal::Group(_)).then(|| info.to.name.clone()) }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn share_by_id(st: &AppState, id: i64) -> ApiResult<ShareRow> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SHARE_COLS} FROM shares WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
pub struct ShareChange {
    pub role: Option<String>,
    /// `null` removes the expiry date.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub keep_expiry: bool,
}

pub async fn update(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<ShareChange>,
) -> ApiResult<Json<ShareInfo>> {
    let s = share_by_id(&st, id).await?;
    let a = access::require(&st.db, me.id, s.node_id, Role::Manager).await?;
    let role = match &req.role {
        Some(r) => grantable(r, a.role)?,
        None => Role::parse(&s.role).unwrap_or(Role::Viewer),
    };
    let expires_at = if req.keep_expiry {
        s.expires_at
    } else {
        future(req.expires_at)?;
        req.expires_at
    };
    let row: ShareRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE shares SET role = $2, expires_at = $3 WHERE id = $1 RETURNING {SHARE_COLS}"
    )))
    .bind(id)
    .bind(role.as_str())
    .bind(expires_at)
    .fetch_one(&st.db)
    .await?;
    audit::log(
        &st.db,
        Some(me.id),
        row.user_id,
        "share_changed",
        Some(&client.ip),
        json!({ "share": id, "node": row.node_id, "role": role, "expires_at": expires_at }),
    )
    .await?;
    let name = a.node.name.clone();
    Ok(Json(share_info(&st, row, a.node.id, name).await?))
}

/// Ends a share: whoever manages the item, or the person it was shared with (to leave it).
pub async fn remove(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let s = share_by_id(&st, id).await?;
    if s.user_id != Some(me.id) {
        access::require(&st.db, me.id, s.node_id, Role::Manager).await?;
    }
    sqlx::query("DELETE FROM shares WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    audit::log(
        &st.db,
        Some(me.id),
        s.user_id,
        "share_removed",
        Some(&client.ip),
        json!({ "share": id, "node": s.node_id }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct SharedItem {
    #[serde(flatten)]
    pub node: NodeInfo,
    pub role: Role,
    /// Whose it is: the owner of the home, or the shared root's name.
    pub owner: String,
    pub shared_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub shared_at: OffsetDateTime,
}

/// What others shared with me (outside roots I see as a whole), newest first.
pub async fn shared_with_me(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<SharedItem>>> {
    let scope = access::scope(&st.db, me.id).await?;
    let nodes: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE id = ANY($1)"
    )))
    .bind(&scope.shared)
    .fetch_all(&st.db)
    .await?;
    let roles = access::roles(&st.db, me.id, &nodes).await?;
    let mut out = Vec::new();
    for n in &nodes {
        let Some(role) = roles.get(&n.id).copied() else {
            continue;
        };
        let (owner, shared_by, shared_at): (String, Option<String>, OffsetDateTime) =
            sqlx::query_as(
                "SELECT coalesce(u.display_name, r.name), b.display_name, s.created_at
                   FROM shares s
                   JOIN nodes n ON n.id = s.node_id
                   JOIN roots r ON r.id = n.root_id
                   LEFT JOIN users u ON u.id = r.owner_user_id AND r.kind = 'home'
                   LEFT JOIN users b ON b.id = s.created_by
                  WHERE s.node_id = $1
                    AND (s.user_id = $2
                         OR s.group_id IN (SELECT group_id FROM group_members WHERE user_id = $2))
                  ORDER BY s.created_at DESC LIMIT 1",
            )
            .bind(n.id)
            .bind(me.id)
            .fetch_one(&st.db)
            .await?;
        out.push(SharedItem {
            node: NodeInfo::from(n),
            role,
            owner,
            shared_by,
            shared_at,
        });
    }
    out.sort_by_key(|i| std::cmp::Reverse(i.shared_at));
    Ok(Json(out))
}
