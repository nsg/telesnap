use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State, rejection::QueryRejection},
    http::{
        HeaderMap, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::AppError, snap::SnapManager};

const AGENT_DOCS: &str = include_str!("../docs.md");
const LANDING_PAGE: &str = include_str!("../landing.html");

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
struct InstallQuery {
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
        .route("/", get(landing))
        .route("/health", get(health))
        .route("/docs.md", get(docs))
        .merge(protected)
        .with_state(state)
}

async fn landing() -> Html<&'static str> {
    Html(LANDING_PAGE)
}

async fn docs() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/markdown; charset=utf-8")], AGENT_DOCS)
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
    query: Result<Query<InstallQuery>, QueryRejection>,
    request: Request,
) -> Result<impl IntoResponse, AppError> {
    let Query(query) =
        query.map_err(|error| AppError::BadRequest(format!("invalid install query: {error}")))?;
    let content_length = parse_content_length(request.headers())?;
    let body: Body = request.into_body();
    let result = state
        .manager
        .install(body, content_length, query.lifetime_seconds)
        .await?;
    Ok((StatusCode::CREATED, Json(result)))
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, AppError> {
    headers
        .get(CONTENT_LENGTH)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| AppError::BadRequest("invalid Content-Length header".to_owned()))?
                .parse::<u64>()
                .map_err(|_| AppError::BadRequest("invalid Content-Length header".to_owned()))
        })
        .transpose()
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
    use axum::{
        body::to_bytes,
        http::{
            HeaderMap, HeaderValue, StatusCode,
            header::{CONTENT_LENGTH, CONTENT_TYPE},
        },
        response::IntoResponse,
    };

    use super::{
        AGENT_DOCS, LANDING_PAGE, constant_time_equal, docs, health_response, landing,
        parse_content_length,
    };

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

    #[test]
    fn parses_optional_content_length() {
        let mut headers = HeaderMap::new();
        assert_eq!(parse_content_length(&headers).unwrap(), None);

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1073741824"));
        assert_eq!(parse_content_length(&headers).unwrap(), Some(1_073_741_824));

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("not-a-number"));
        assert!(parse_content_length(&headers).is_err());
    }

    #[tokio::test]
    async fn serves_public_discovery_pages_with_correct_types() {
        let landing = landing().await.into_response();
        assert_eq!(landing.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        let landing_body = to_bytes(landing.into_body(), LANDING_PAGE.len())
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&landing_body).contains("href=\"/docs.md\""));

        let docs = docs().await.into_response();
        assert_eq!(docs.headers()[CONTENT_TYPE], "text/markdown; charset=utf-8");
        let docs_body = to_bytes(docs.into_body(), AGENT_DOCS.len()).await.unwrap();
        assert!(String::from_utf8_lossy(&docs_body).contains("--data-binary"));
    }
}
