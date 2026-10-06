//! Hop-by-hop header filtering, accept-encoding: identity (spec section 8.3). WI-07.
//!
//! Request headers forwarded: all except the hop-by-hop set (matched
//! case-insensitively). Additionally on requests: `accept-encoding` is forced
//! to `identity` (upstream responses stay byte-exact, SSE included), and
//! `content-length` is dropped (the body may be rewritten by model
//! canonicalization; the HTTP client recomputes it).

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};

use crate::auth::{PROXY_KEY_HEADER, ProxyKeySource};

/// Hop-by-hop headers never forwarded (spec section 8.3).
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// True when the (lowercase) header name is in the hop-by-hop set.
pub fn is_hop_by_hop(lower_name: &str) -> bool {
    HOP_BY_HOP.contains(&lower_name)
}

/// Build the forwarded request headers (spec section 8.3, pipeline step 2):
/// drop hop-by-hop, strip the proxy credential, and (when the proxy key came
/// via `Authorization`) strip `authorization` so it cannot leak to the model
/// server. `accept-encoding` is forced to `identity`.
pub fn build_forward_headers(
    incoming: &HeaderMap,
    key_source: Option<ProxyKeySource>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in incoming {
        let lower = name.as_str();
        if is_hop_by_hop(lower) || lower == PROXY_KEY_HEADER {
            continue;
        }
        if lower == "authorization" && key_source == Some(ProxyKeySource::Authorization) {
            continue;
        }
        out.append(name, value.clone());
    }
    out.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    out
}

/// Merge upstream auth headers into the forwarded set (pipeline step 9). The
/// upstream key overrides any client `authorization` that survived step 2.
pub fn merge_pod_auth(headers: &mut HeaderMap, pod_auth: &[(String, String)]) {
    for (name, value) in pod_auth {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    }
}

/// Filter the upstream response headers for relay (spec section 8.3): drop the
/// hop-by-hop set, keep everything else (multi-values preserved).
pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        out.append(name, value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn strips_hop_by_hop_headers() {
        let out = build_forward_headers(
            &map(&[
                ("connection", "close"),
                ("host", "example.com"),
                ("content-length", "5"),
                ("x-api-key", "secret"),
            ]),
            None,
        );
        assert!(out.get("connection").is_none());
        assert!(out.get("host").is_none());
        assert!(out.get("content-length").is_none());
        assert_eq!(out.get("x-api-key").unwrap(), "secret");
    }

    #[test]
    fn forces_accept_encoding_identity() {
        let out = build_forward_headers(&map(&[("accept-encoding", "gzip, br")]), None);
        assert_eq!(out.get(header::ACCEPT_ENCODING).unwrap(), "identity");
    }

    #[test]
    fn adds_accept_encoding_when_absent() {
        let out = build_forward_headers(&map(&[("x-a", "b")]), None);
        assert_eq!(out.get(header::ACCEPT_ENCODING).unwrap(), "identity");
    }

    #[test]
    fn strips_proxy_key_header() {
        let out = build_forward_headers(&map(&[(PROXY_KEY_HEADER, "pk")]), None);
        assert!(out.get(PROXY_KEY_HEADER).is_none());
    }

    #[test]
    fn strips_authorization_when_key_came_via_bearer() {
        let out = build_forward_headers(
            &map(&[("authorization", "Bearer pk")]),
            Some(ProxyKeySource::Authorization),
        );
        assert!(out.get("authorization").is_none());
    }

    #[test]
    fn keeps_authorization_when_key_came_via_header() {
        let out = build_forward_headers(
            &map(&[("authorization", "Bearer client-token")]),
            Some(ProxyKeySource::Header),
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer client-token");
    }

    #[test]
    fn keeps_authorization_when_no_proxy_key() {
        let out = build_forward_headers(&map(&[("authorization", "Bearer t")]), None);
        assert_eq!(out.get("authorization").unwrap(), "Bearer t");
    }

    #[test]
    fn merge_pod_auth_overrides_client_authorization() {
        let mut out = build_forward_headers(&map(&[("authorization", "Bearer client")]), None);
        merge_pod_auth(
            &mut out,
            &[("authorization".into(), "Bearer upstream".into())],
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer upstream");
    }
}
