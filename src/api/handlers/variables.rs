//! Operator endpoints for reusable org variables + the secret credential store.
//!
//! Mounted under `/api/v1/variables`, gated by the `Authorized<Variables…>`
//! extractor so a variable is only ever visible to its owning org. A secret
//! variable's value is sealed at rest by the store and never serialized — every
//! read path returns `value: null` for secrets. `used_by` reports how many
//! monitors reference a variable, and a referenced variable cannot be deleted.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::{NewVariable, Variable, VariableId, validate_var_key};
use crate::error::{AppError, Result};
use crate::storage::CreateVariableOutcome;
use crate::web::{Authorized, CurrentUser, VariablesRead, VariablesWrite};

/// A variable plus its blast radius. The flattened [`Variable`] already redacts
/// a secret's value (`null`); `used_by` is the count of referencing monitors.
#[derive(Debug, Serialize, ToSchema)]
pub struct VariableView {
    #[serde(flatten)]
    variable: Variable,
    used_by: i64,
}

/// PATCH body: rotate a variable's value. The secret flag is fixed at create.
#[derive(Debug, Deserialize, ToSchema)]
pub struct VariableValueUpdate {
    pub value: String,
}

fn view(variable: Variable, used_by: i64) -> VariableView {
    VariableView { variable, used_by }
}

#[utoipa::path(
    get,
    path = "/api/v1/variables",
    tag = "variables",
    summary = "List variables",
    responses((status = 200, body = [VariableView])),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<VariablesRead>,
) -> Result<Json<Vec<VariableView>>> {
    let (vars, counts) = tokio::try_join!(
        state.variable_store.list(org),
        state.variable_store.usage_counts(org),
    )?;
    Ok(Json(
        vars.into_iter()
            .map(|v| {
                let used_by = counts.get(&v.key).copied().unwrap_or(0);
                view(v, used_by)
            })
            .collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/variables/{id}",
    tag = "variables",
    summary = "Get a variable",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = VariableView), (status = 404, body = ApiError)),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<VariablesRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<VariableView>> {
    let var = state
        .variable_store
        .get(org, VariableId(id))
        .await?
        .ok_or_else(not_found)?;
    let used_by = used_by_for(&state, org, &var.key).await?;
    Ok(Json(view(var, used_by)))
}

#[utoipa::path(
    post,
    path = "/api/v1/variables",
    tag = "variables",
    summary = "Create a variable",
    request_body = NewVariable,
    responses(
        (status = 201, body = VariableView,
            headers(("Location" = String, description = "URL of the new variable"))),
        (status = 400, body = ApiError, description = "Invalid key"),
        (status = 409, body = ApiError, description = "Key already in use in this org"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<VariablesWrite>,
    CurrentUser(user): CurrentUser,
    Json(new): Json<NewVariable>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Json<VariableView>,
)> {
    validate_var_key(&new.key)
        .map_err(|e| AppError::bad_request(codes::INVALID_VARIABLE_KEY, e.to_string()))?;
    let var = match state.variable_store.create(org, new, Some(user)).await? {
        CreateVariableOutcome::Created(v) => v,
        CreateVariableOutcome::DuplicateKey => {
            return Err(AppError::conflict(
                codes::VARIABLE_KEY_EXISTS,
                "a variable with this key already exists",
            ));
        }
    };
    let location = HeaderValue::from_str(&format!("/api/v1/variables/{}", var.id))
        .expect("uuid produces ascii-only path");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Json(view(var, 0)),
    ))
}

#[utoipa::path(
    patch,
    path = "/api/v1/variables/{id}",
    tag = "variables",
    summary = "Rotate a variable's value",
    params(("id" = Uuid, Path)),
    request_body = VariableValueUpdate,
    responses((status = 200, body = VariableView), (status = 404, body = ApiError)),
)]
pub async fn update(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<VariablesWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<VariableValueUpdate>,
) -> Result<Json<VariableView>> {
    let var = state
        .variable_store
        .update_value(org, VariableId(id), &body.value, Some(user))
        .await?
        .ok_or_else(not_found)?;
    let used_by = used_by_for(&state, org, &var.key).await?;
    Ok(Json(view(var, used_by)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/variables/{id}",
    tag = "variables",
    summary = "Delete a variable",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "Variable is referenced by a monitor"),
    ),
)]
pub async fn delete(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<VariablesWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let var = state
        .variable_store
        .get(org, VariableId(id))
        .await?
        .ok_or_else(not_found)?;
    let used_by = used_by_for(&state, org, &var.key).await?;
    if used_by > 0 {
        return Err(AppError::conflict(
            codes::VARIABLE_IN_USE,
            format!("variable is referenced by {used_by} monitor(s); update them first"),
        ));
    }
    state
        .variable_store
        .delete(org, VariableId(id), Some(user))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn used_by_for(state: &AppState, org: crate::domain::OrgId, key: &str) -> Result<i64> {
    state.variable_store.usage_count(org, key).await
}

fn not_found() -> AppError {
    AppError::not_found(codes::VARIABLE_NOT_FOUND, "variable not found")
}
