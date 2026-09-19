//! Server-rendered variables pages under `/settings/variables`: a list with
//! inline value-edit and delete row actions. Mutations run from the page
//! against the JSON API (`/api/v1/variables`), so this module only renders
//! chrome and the current rows.
//!
//! A secret variable's value never reaches the browser: the store returns
//! `value: None` for secrets, the row shows `••••`, and the edit field starts
//! empty (write-only rotate). A variable referenced by a monitor can't be
//! deleted — the API returns 409 and the row's delete surfaces it.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::error::AppError;
use crate::request::CurrentOrg;
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::resolve_org;

const TAB_VARIABLES: &str = "variables";

pub struct VarRow {
    pub id: String,
    pub key: String,
    pub is_secret: bool,
    /// Plain text for a plain variable; empty for a secret (never decrypted here).
    pub value: String,
    pub used_by: i64,
    pub updated: chrono::DateTime<chrono::Utc>,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/variables.html")]
pub struct VariablesPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/variables_partial.html")]
pub struct VariablesPartial {
    pub variables: Vec<VarRow>,
}

pub async fn index(org: Result<CurrentOrg, AppError>) -> Response {
    match resolve_org(org, "/settings/variables") {
        Ok(_) => VariablesPage {
            active_tab: TAB_VARIABLES,
        }
        .into_response(),
        Err(resp) => *resp,
    }
}

pub async fn list_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/variables") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let (vars, counts) = tokio::try_join!(
        state.variable_store.list(org),
        state.variable_store.usage_counts(org),
    )?;
    let mut variables: Vec<VarRow> = vars
        .into_iter()
        .map(|v| VarRow {
            used_by: counts.get(&v.key).copied().unwrap_or(0),
            id: v.id.to_string(),
            value: v.value.unwrap_or_default(),
            is_secret: v.is_secret,
            updated: v.updated_at,
            key: v.key,
        })
        .collect();
    variables.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(VariablesPartial { variables }.into_response())
}
