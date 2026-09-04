use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication is required")]
    Unauthorized,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{operation} failed: {message}")]
    Command {
        operation: &'static str,
        message: String,
    },
    #[error("{operation} timed out after {seconds} seconds")]
    CommandTimeout {
        operation: &'static str,
        seconds: u64,
    },
    #[error("download failed: {0}")]
    Download(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_managed"),
            Self::Command { .. } => (StatusCode::BAD_GATEWAY, "snap_command_failed"),
            Self::CommandTimeout { .. } => (StatusCode::GATEWAY_TIMEOUT, "command_timeout"),
            Self::Download(_) => (StatusCode::BAD_GATEWAY, "download_failed"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let response = ErrorEnvelope {
            error: ErrorBody {
                code,
                message: self.to_string(),
            },
        };
        (status, Json(response)).into_response()
    }
}

pub fn concise_output(output: &[u8]) -> String {
    const LIMIT: usize = 8 * 1024;
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    if text.len() <= LIMIT {
        text.to_owned()
    } else {
        format!("{}…", &text[..text.floor_char_boundary(LIMIT)])
    }
}
