use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("r2d2 error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("time parse error: {0}")]
    TimeParse(#[from] chrono::ParseError),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::TimeParse(_) => (
                StatusCode::BAD_REQUEST,
                format!("invalid time format: {self}"),
            ),
            AppError::Db(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            ),
            AppError::Pool(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("connection pool error: {e}"),
            ),
            AppError::Serde(e) => (
                StatusCode::BAD_REQUEST,
                format!("invalid json: {e}"),
            ),
            AppError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
        };

        let body = Json(json!({
            "error": status.to_string(),
            "message": message,
        }));

        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
