//! X-Request-Id echo/generate (spec section 8.1). WI-07.
//!
//! Honors a well-formed client-supplied `x-request-id` (1..=128 chars) so
//! callers can correlate their own logs with ours; otherwise generates a
//! random id. The id is echoed on every response the app produces (including
//! auth failures), so it must sit outside the auth middleware.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

/// The header carrying the correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Max length of a client-supplied request id we will honor.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Pick the request id: honor a well-formed client-supplied one, else generate
/// a random 32-hex-char id (parity with Python `uuid.uuid4().hex`).
pub fn resolve_request_id(incoming: &str) -> String {
    let trimmed = incoming.trim();
    if (1..=MAX_REQUEST_ID_LEN).contains(&trimmed.len()) {
        trimmed.to_string()
    } else {
        uuid::Uuid::new_v4().simple().to_string()
    }
}

/// Echo/generate the `x-request-id` on every response.
pub async fn request_id_middleware(req: Request, next: Next) -> Response {
    let incoming = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let rid = resolve_request_id(incoming);
    let mut resp = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&rid) {
        resp.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honors_well_formed_client_id() {
        assert_eq!(resolve_request_id("abc-123"), "abc-123");
    }

    #[test]
    fn trims_client_id() {
        assert_eq!(resolve_request_id("  abc  "), "abc");
    }

    #[test]
    fn generates_when_empty() {
        let id = resolve_request_id("");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generates_when_too_long() {
        let long = "x".repeat(129);
        let id = resolve_request_id(&long);
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn honors_max_length() {
        let max = "x".repeat(128);
        assert_eq!(resolve_request_id(&max), max);
    }

    #[test]
    fn generated_ids_are_unique() {
        let a = resolve_request_id("");
        let b = resolve_request_id("");
        assert_ne!(a, b);
    }
}
