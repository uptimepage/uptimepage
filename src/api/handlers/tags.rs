use crate::api::json::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::api::ApiError;
use crate::api::page::{PageEnvelope, PageOfTagCount};
use crate::app::AppState;
use crate::error::Result;
use crate::web::CurrentOrg;

const TAGS_LIMIT_DEFAULT: usize = 100;
const TAGS_LIMIT_MAX: usize = 1_000;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TagsQuery {
    /// Prefix-filter tag names (for autocomplete).
    pub q: Option<String>,
    /// Max items (default 100, max 1000).
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tags",
    tag = "tags",
    summary = "List every tag currently in use with target count",
    description = "Sorted by descending count, then alphabetical. Tags assigned only to disabled targets are still included.",
    params(TagsQuery),
    responses(
        (status = 200, body = PageOfTagCount, example = json!({
            "items": [{"name": "prod", "count": 12}, {"name": "staging", "count": 4}],
            "limit": 100, "offset": 0, "has_more": true
        })),
        (status = 400, body = ApiError),
    ),
)]
pub async fn list_tags(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    Query(q): Query<TagsQuery>,
) -> Result<Json<PageOfTagCount>> {
    let limit = q.limit.unwrap_or(TAGS_LIMIT_DEFAULT).min(TAGS_LIMIT_MAX);
    let peek = state
        .target_store
        .list_tags(org, q.q.clone(), limit + 1)
        .await?;
    Ok(Json(PageEnvelope::from_peek(peek, limit as u32, 0)))
}
