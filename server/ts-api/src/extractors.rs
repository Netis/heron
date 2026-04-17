//! Custom Axum extractors that convert rejection errors into `ApiError` so
//! every failure — including malformed query strings or path params — is
//! serialized through the shared `{ code, message, data }` envelope instead
//! of Axum's default plain-text rejection body.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::response::ApiError;

pub struct Query<T>(pub T);

impl<S, T> FromRequestParts<S> for Query<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Query(v)),
            Err(e) => Err(ApiError::InvalidParam(format!("invalid query: {e}"))),
        }
    }
}

pub struct Path<T>(pub T);

impl<S, T> FromRequestParts<S> for Path<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(v)) => Ok(Path(v)),
            Err(e) => Err(ApiError::InvalidParam(format!("invalid path parameter: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use serde::Deserialize;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::response::ApiResponse;

    #[derive(Deserialize)]
    struct ReqParams {
        #[allow(dead_code)]
        start: i64,
    }

    async fn need_query(Query(_): Query<ReqParams>) -> axum::response::Response {
        use axum::response::IntoResponse;
        ApiResponse::ok(()).into_response()
    }

    async fn need_u64_path(Path(_): Path<u64>) -> axum::response::Response {
        use axum::response::IntoResponse;
        ApiResponse::ok(()).into_response()
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn query_rejection_returns_json_envelope() {
        let app: Router = Router::new().route("/q", get(need_query));

        let resp = app
            .oneshot(Request::builder().uri("/q").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let v = body_json(resp).await;
        assert_eq!(v["code"], 1001);
        assert!(
            v["message"].as_str().unwrap().contains("invalid query"),
            "message: {}",
            v["message"]
        );
        assert!(v["data"].is_object());
    }

    #[tokio::test]
    async fn path_rejection_returns_json_envelope() {
        let app: Router = Router::new().route("/p/{id}", get(need_u64_path));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/p/not-a-number")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let v = body_json(resp).await;
        assert_eq!(v["code"], 1001);
        assert!(
            v["message"].as_str().unwrap().contains("invalid path"),
            "message: {}",
            v["message"]
        );
    }

    #[tokio::test]
    async fn valid_query_passes_through() {
        let app: Router = Router::new().route("/q", get(need_query));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/q?start=42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["code"], 0);
        assert_eq!(v["message"], "ok");
    }
}

