//! Mutable upstream selected at runtime (spec section 6). WI-06.
//!
//! The proxy's upstream can change at runtime (a discovered/created pod
//! replaces the configured one). The URL is stored without a trailing slash;
//! `warmup_url` appends the warmup path.

/// The currently-selected upstream (URL + optional pod id).
#[derive(Debug, Clone)]
pub struct UpstreamTarget {
    url: String,
    pod_id: String,
}

impl UpstreamTarget {
    /// Build a target, trimming any trailing slash from the URL.
    pub fn new(url: &str, pod_id: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            pod_id: pod_id.to_string(),
        }
    }

    /// The upstream base URL (no trailing slash).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The pod id (empty in serverless mode).
    pub fn pod_id(&self) -> &str {
        &self.pod_id
    }

    /// Replace the upstream (trims trailing slash from the URL).
    pub fn set(&mut self, url: &str, pod_id: &str) {
        self.url = url.trim_end_matches('/').to_string();
        self.pod_id = pod_id.to_string();
    }

    /// The warmup/health probe URL for the given warmup path.
    pub fn warmup_url(&self, warmup_path: &str) -> String {
        let path = warmup_path.trim_start_matches('/');
        if path.is_empty() {
            self.url.clone()
        } else {
            format!("{}/{}", self.url, path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_trailing_slash() {
        let t = UpstreamTarget::new("http://localhost:9000/", "pod-1");
        assert_eq!(t.url(), "http://localhost:9000");
        assert_eq!(t.pod_id(), "pod-1");
    }

    #[test]
    fn set_replaces_and_trims() {
        let mut t = UpstreamTarget::new("http://a", "");
        t.set("http://b/", "pod-2");
        assert_eq!(t.url(), "http://b");
        assert_eq!(t.pod_id(), "pod-2");
    }

    #[test]
    fn warmup_url_appends_path_without_double_slash() {
        let t = UpstreamTarget::new("http://localhost:9000", "");
        assert_eq!(t.warmup_url("v1/models"), "http://localhost:9000/v1/models");
        // Leading slash on the path is stripped.
        assert_eq!(
            t.warmup_url("/v1/models"),
            "http://localhost:9000/v1/models"
        );
    }

    #[test]
    fn warmup_url_empty_path_returns_base() {
        let t = UpstreamTarget::new("http://localhost:9000", "");
        assert_eq!(t.warmup_url(""), "http://localhost:9000");
        assert_eq!(t.warmup_url("/"), "http://localhost:9000");
    }
}
