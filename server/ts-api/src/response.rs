use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub code: i32,
    pub message: String,
    pub data: T,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            code: 0,
            message: "ok".to_string(),
            data,
        }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        let body = serde_json::to_string(&self).unwrap_or_else(|e| {
            format!(r#"{{"code":5001,"message":"serialization error: {e}","data":{{}}}}"#)
        });
        (StatusCode::OK, [("content-type", "application/json")], body).into_response()
    }
}

#[derive(Debug)]
pub enum ApiError {
    InvalidParam(String),
    NotFound(String),
    Internal(String),
}

#[derive(Serialize)]
struct EmptyObject {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::InvalidParam(msg) => (StatusCode::BAD_REQUEST, 1001, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, 2001, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, 5001, msg),
        };
        let body = ApiResponse {
            code,
            message,
            data: EmptyObject {},
        };
        let json = serde_json::to_string(&body).unwrap_or_else(|e| {
            format!(r#"{{"code":5001,"message":"serialization error: {e}","data":{{}}}}"#)
        });
        (status, [("content-type", "application/json")], json).into_response()
    }
}

impl From<ts_common::error::AppError> for ApiError {
    fn from(err: ts_common::error::AppError) -> Self {
        tracing::error!(error = %err, "internal error handling API request");
        ApiError::Internal("internal server error".to_string())
    }
}
