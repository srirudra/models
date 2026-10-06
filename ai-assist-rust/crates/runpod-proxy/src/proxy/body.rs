//! Streaming body read with the `MAX_BODY_BYTES` cap (413 over cap). WI-07.
//!
//! Reads the request body in a stream instead of buffering it whole, so an
//! oversized upload is rejected with 413 rather than buffered into memory
//! (an unauthenticated OOM vector on a proxy in front of a billable endpoint).
//! The stream is read until it crosses the limit, then rejected.

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;

/// The result of reading a request body with a cap.
pub enum BodyRead {
    /// The body fit within the cap (or the cap is disabled).
    Ok(Vec<u8>),
    /// The body exceeded the cap.
    TooLarge,
}

/// Read the request body, enforcing `limit` bytes (<=0 means unlimited).
pub async fn read_body(body: Body, limit: i64) -> BodyRead {
    if limit <= 0 {
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap_or_default();
        return BodyRead::Ok(bytes.to_vec());
    }
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    let mut stream = body.into_data_stream();
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        total = total.saturating_add(chunk.len() as u64);
        if total > limit {
            return BodyRead::TooLarge;
        }
        chunks.push(chunk);
    }
    let mut out = Vec::with_capacity(usize::try_from(total).unwrap_or(usize::MAX));
    for c in &chunks {
        out.extend_from_slice(c);
    }
    BodyRead::Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[tokio::test]
    async fn reads_body_under_limit() {
        let body = Body::from("hello world");
        match read_body(body, 100).await {
            BodyRead::Ok(bytes) => assert_eq!(bytes, b"hello world".to_vec()),
            BodyRead::TooLarge => panic!("should not be too large"),
        }
    }

    #[tokio::test]
    async fn rejects_body_over_limit() {
        let body = Body::from("hello world");
        match read_body(body, 5).await {
            BodyRead::Ok(_) => panic!("should be too large"),
            BodyRead::TooLarge => {}
        }
    }

    #[tokio::test]
    async fn body_exactly_at_limit_is_ok() {
        let body = Body::from("12345");
        match read_body(body, 5).await {
            BodyRead::Ok(bytes) => assert_eq!(bytes, b"12345".to_vec()),
            BodyRead::TooLarge => panic!("exactly at limit should be ok"),
        }
    }

    #[tokio::test]
    async fn unlimited_reads_whole_body() {
        let body = Body::from("some data");
        match read_body(body, 0).await {
            BodyRead::Ok(bytes) => assert_eq!(bytes, b"some data".to_vec()),
            BodyRead::TooLarge => panic!("unlimited should not be too large"),
        }
    }

    #[tokio::test]
    async fn empty_body_is_ok() {
        let body = Body::empty();
        match read_body(body, 100).await {
            BodyRead::Ok(bytes) => assert!(bytes.is_empty()),
            BodyRead::TooLarge => panic!("empty body should be ok"),
        }
    }
}
