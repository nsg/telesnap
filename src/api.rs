use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::AppError, snap::SnapManager};

#[derive(Clone)]
struct AppState {
    manager: Arc<SnapManager>,
    token: Arc<str>,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

#[derive(Deserialize)]
struct InstallRequest {
    url: String,
    lifetime_seconds: u64,
}

#[derive(Deserialize)]
struct SetConfigRequest {
    value: Value,
}

#[derive(Debug, Default, Deserialize)]
struct TargetQuery {
    service: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LogsQuery {
    service: Option<String>,
    lines: Option<u16>,
}

pub fn router(manager: Arc<SnapManager>, token: String) -> Router {
    let state = AppState {
        manager,
        token: token.into(),
    };
    let protected = Router::new()
        .route("/v1/snaps", get(list_managed))
        .route("/v1/snaps/install", post(install))
        .route("/v1/snaps/{snap}", delete(remove))
        .route("/v1/snaps/{snap}/purge", delete(purge))
        .route("/v1/snaps/{snap}/config", get(get_all_config))
        .route(
            "/v1/snaps/{snap}/config/{*key}",
            get(get_config).put(set_config).delete(unset_config),
        )
        .route("/v1/snaps/{snap}/logs", get(logs))
        .route("/v1/snaps/{snap}/services/start", post(start))
        .route("/v1/snaps/{snap}/services/stop", post(stop))
        .route("/v1/snaps/{snap}/services/restart", post(restart))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    health_response(state.manager.is_ready())
}

fn health_response(ready: bool) -> (StatusCode, Json<Health>) {
    if ready {
        (StatusCode::OK, Json(Health { status: "ok" }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Health {
                status: "unavailable",
            }),
        )
    }
}

async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied.is_some_and(|value| constant_time_equal(value.as_bytes(), state.token.as_bytes())) {
        Ok(next.run(request).await)
    } else {
        Err(AppError::Unauthorized)
    }
}

async fn list_managed(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.manager.list_managed().await)
}

async fn install(
    State(state): State<AppState>,
    Json(request): Json<InstallRequest>,
) -> Result<impl IntoResponse, AppError> {
    let url = Url::parse(&request.url)
        .map_err(|error| AppError::BadRequest(format!("invalid url: {error}")))?;
    let result = state.manager.install(url, request.lifetime_seconds).await?;
    Ok((StatusCode::CREATED, Json(result)))
}

async fn remove(
    State(state): State<AppState>,
    Path(snap): Path<String>,
) -> Result<StatusCode, AppError> {
    state.manager.remove(&snap, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn purge(
    State(state): State<AppState>,
    Path(snap): Path<String>,
) -> Result<StatusCode, AppError> {
    state.manager.remove(&snap, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_all_config(
    State(state): State<AppState>,
    Path(snap): Path<String>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(state.manager.get_config(&snap, None).await?))
}

async fn get_config(
    State(state): State<AppState>,
    Path((snap, key)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(state.manager.get_config(&snap, Some(&key)).await?))
}

async fn set_config(
    State(state): State<AppState>,
    Path((snap, key)): Path<(String, String)>,
    Json(request): Json<SetConfigRequest>,
) -> Result<StatusCode, AppError> {
    state.manager.set_config(&snap, &key, request.value).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unset_config(
    State(state): State<AppState>,
    Path((snap, key)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    state.manager.unset_config(&snap, &key).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn logs(
    State(state): State<AppState>,
    Path(snap): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let lines = query.lines.unwrap_or(200);
    if !(1..=1000).contains(&lines) {
        return Err(AppError::BadRequest(
            "lines must be between 1 and 1000".to_owned(),
        ));
    }
    Ok(Json(
        state
            .manager
            .logs(&snap, query.service.as_deref(), lines)
            .await?,
    ))
}

async fn start(
    state: State<AppState>,
    path: Path<String>,
    query: Query<TargetQuery>,
) -> Result<impl IntoResponse, AppError> {
    service_action(state, path, query, "start").await
}

async fn stop(
    state: State<AppState>,
    path: Path<String>,
    query: Query<TargetQuery>,
) -> Result<impl IntoResponse, AppError> {
    service_action(state, path, query, "stop").await
}

async fn restart(
    state: State<AppState>,
    path: Path<String>,
    query: Query<TargetQuery>,
) -> Result<impl IntoResponse, AppError> {
    service_action(state, path, query, "restart").await
}

async fn service_action(
    State(state): State<AppState>,
    Path(snap): Path<String>,
    Query(query): Query<TargetQuery>,
    action: &'static str,
) -> Result<impl IntoResponse, AppError> {
    Ok(Json(
        state
            .manager
            .service_action(&snap, query.service.as_deref(), action)
            .await?,
    ))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse};

    use super::{constant_time_equal, health_response};

    #[test]
    fn compares_tokens() {
        assert!(constant_time_equal(b"correct", b"correct"));
        assert!(!constant_time_equal(b"correct", b"wrong"));
        assert!(!constant_time_equal(b"short", b"shorter"));
    }

    #[test]
    fn health_reflects_cached_snapd_readiness() {
        assert_eq!(
            health_response(true).into_response().status(),
            StatusCode::OK
        );
        assert_eq!(
            health_response(false).into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
