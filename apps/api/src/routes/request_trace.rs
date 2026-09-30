//! Per-request correlation: every request gets a `request_id` that appears on
//! every log line it produces and comes back in the response header, so a
//! caller can quote an id and an operator can pull the whole request out of
//! the logs.
//!
//! An inbound `X-Request-Id` is honoured rather than replaced, so an id
//! assigned by an upstream proxy — or by the outpost that originated a fabric
//! call — survives the hop.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;
use uuid::Uuid;

use crate::services::metrics::{incr, METRICS};

pub const HEADER_REQUEST_ID: &str = "x-request-id";

/// Cap on an accepted upstream id. Without this, an attacker-controlled
/// header of arbitrary length lands in every log line for that request.
const MAX_INBOUND_ID_LEN: usize = 128;

pub async fn trace_requests(req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get(HEADER_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= MAX_INBOUND_ID_LEN)
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    let span = tracing::info_span!(
        "http",
        request_id = %request_id,
        method = %method,
        path = %path,
        status = tracing::field::Empty,
    );

    incr(&METRICS.http_requests);

    async move {
        let mut response = next.run(req).await;
        let status = response.status();
        tracing::Span::current().record("status", status.as_u16());

        if status.is_server_error() {
            tracing::error!(status = status.as_u16(), "request failed");
        } else if status.is_client_error() {
            if status == axum::http::StatusCode::UNAUTHORIZED
                || status == axum::http::StatusCode::FORBIDDEN
            {
                incr(&METRICS.auth_failures);
            }
            tracing::warn!(status = status.as_u16(), "request rejected");
        } else {
            tracing::info!(status = status.as_u16(), "request completed");
        }

        // Echo the id back so a caller can correlate without reading logs.
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(HEADER_REQUEST_ID), value);
        }
        response
    }
    .instrument(span)
    .await
}
