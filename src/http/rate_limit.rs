/// Per-IP token bucket rate limiter for the HTTP API.
///
/// Uses a `papaya::HashMap` for concurrent access. Each IP address gets a
/// token bucket that tracks request count within a sliding window. When the
/// window expires, the bucket resets.
///
/// Expired entries are cleaned up periodically to prevent unbounded memory
/// growth from one-off clients.
use std::sync::Arc;
use std::time::Instant;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use papaya::HashMap as ConcurrentHashMap;
use papaya::Operation;
use serde::Serialize;

use super::server::AppState;

/// Response body returned when rate limited.
#[derive(Serialize)]
struct RateLimitResponse {
    ok: bool,
    error: String,
}

/// A token bucket tracking requests within a time window.
#[derive(Clone)]
struct TokenBucket {
    /// Number of requests made in the current window.
    count: u64,
    /// Start time of the current window.
    window_start: Instant,
}

/// Shared rate limiter state. Thread-safe via papaya HashMap.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<ConcurrentHashMap<String, TokenBucket>>,
    max_requests: u64,
    window_ms: u64,
}

impl RateLimiter {
    /// Create a new rate limiter. If max_requests is 0, rate limiting is disabled.
    pub fn new(max_requests: u64, window_ms: u64) -> Self {
        Self {
            buckets: Arc::new(ConcurrentHashMap::new()),
            max_requests,
            window_ms: if window_ms == 0 { 1000 } else { window_ms },
        }
    }

    /// Check if a request from the given IP is allowed.
    /// Returns true if allowed, false if rate limited.
    pub fn check(&self, ip: &str) -> bool {
        // If rate limiting is disabled, always allow
        if self.max_requests == 0 {
            return true;
        }

        let now = Instant::now();
        let window_duration = std::time::Duration::from_millis(self.window_ms);
        let max_requests = self.max_requests;
        let key = ip.to_string();

        // Use papaya's compute for atomic read-modify-write.
        // The closure may be called multiple times under contention (CAS retries),
        // so it must be pure and deterministic.
        let pinned = self.buckets.pin();
        let result = pinned.compute(key, |entry| match entry {
            Some((_, existing)) => {
                let mut bucket = existing.clone();
                // Reset window if expired
                if now.duration_since(bucket.window_start) >= window_duration {
                    bucket.count = 0;
                    bucket.window_start = now;
                }
                if bucket.count >= max_requests {
                    // Rate limited - abort without updating the bucket.
                    // We use Abort to signal "don't change anything" while
                    // communicating that the request is denied.
                    Operation::Abort(false)
                } else {
                    bucket.count += 1;
                    Operation::Insert(bucket)
                }
            }
            None => {
                // First request from this IP - allow it
                Operation::Insert(TokenBucket {
                    count: 1,
                    window_start: now,
                })
            }
        });

        match result {
            papaya::Compute::Inserted(_, _) => true,
            papaya::Compute::Updated { .. } => true,
            papaya::Compute::Removed(_, _) => false,
            papaya::Compute::Aborted(denied) => denied,
        }
    }

    /// Remove expired entries to prevent unbounded memory growth.
    /// Call this periodically (e.g., every window duration).
    pub fn cleanup_expired(&self) {
        if self.max_requests == 0 {
            return;
        }

        let now = Instant::now();
        let window_duration = std::time::Duration::from_millis(self.window_ms);
        let guard = self.buckets.guard();

        self.buckets.retain(
            |_, bucket| now.duration_since(bucket.window_start) < window_duration,
            &guard,
        );
    }

    /// Returns true if rate limiting is disabled.
    pub fn is_disabled(&self) -> bool {
        self.max_requests == 0
    }
}

/// Extract the client IP from the request.
/// Checks X-Forwarded-For and X-Real-IP headers first (for reverse proxy
/// setups), then falls back to the connection info.
fn extract_client_ip(request: &Request) -> String {
    // Check X-Forwarded-For header (first IP in the list is the original client)
    if let Some(xff) = request.headers().get("x-forwarded-for")
        && let Ok(val) = xff.to_str()
        && let Some(ip) = val.split(',').next()
    {
        let ip = ip.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }

    // Check X-Real-IP header
    if let Some(xri) = request.headers().get("x-real-ip")
        && let Ok(val) = xri.to_str()
    {
        let ip = val.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }

    // Fall back to axum's ConnectInfo extension
    if let Some(connect_info) = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        return connect_info.0.ip().to_string();
    }

    // Last resort: unknown IP. All requests without identifiable IPs share
    // the same bucket. This is conservative (limits all such requests together)
    // but prevents bypass by omitting headers.
    "unknown".to_string()
}

/// Axum middleware for per-IP rate limiting.
/// Returns 429 Too Many Requests with a JSON body when rate limited.
pub async fn rate_limit_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> axum::response::Response {
    let rate_limiter = &state.rate_limiter;

    // If rate limiting is disabled, pass through
    if rate_limiter.is_disabled() {
        return next.run(request).await;
    }

    let ip = extract_client_ip(&request);

    if !rate_limiter.check(&ip) {
        let body = RateLimitResponse {
            ok: false,
            error: "rate limit exceeded".to_string(),
        };
        return (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    }

    next.run(request).await
}

/// Start a background task that periodically cleans up expired rate limit
/// entries. Returns a JoinHandle for the cleanup task.
pub fn start_cleanup_task(limiter: RateLimiter, window_ms: u64) -> tokio::task::JoinHandle<()> {
    let cleanup_interval = if window_ms == 0 { 1000 } else { window_ms };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(cleanup_interval)).await;
            limiter.cleanup_expired();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_disabled() {
        let limiter = RateLimiter::new(0, 1000);
        assert!(limiter.is_disabled());
        // Should always allow when disabled
        assert!(limiter.check("1.2.3.4"));
        assert!(limiter.check("1.2.3.4"));
    }

    #[test]
    fn test_rate_limiter_within_limit() {
        let limiter = RateLimiter::new(5, 1000);
        assert!(!limiter.is_disabled());
        for _ in 0..5 {
            assert!(limiter.check("1.2.3.4"));
        }
        // 6th request should be rejected
        assert!(!limiter.check("1.2.3.4"));
    }

    #[test]
    fn test_rate_limiter_per_ip() {
        let limiter = RateLimiter::new(2, 1000);
        assert!(limiter.check("1.2.3.4"));
        assert!(limiter.check("1.2.3.4"));
        assert!(!limiter.check("1.2.3.4")); // limited

        // Different IP should have its own bucket
        assert!(limiter.check("5.6.7.8"));
        assert!(limiter.check("5.6.7.8"));
        assert!(!limiter.check("5.6.7.8")); // limited
    }

    #[test]
    fn test_rate_limiter_window_reset() {
        let limiter = RateLimiter::new(2, 50); // 50ms window
        assert!(limiter.check("1.2.3.4"));
        assert!(limiter.check("1.2.3.4"));
        assert!(!limiter.check("1.2.3.4")); // limited

        // Wait for the window to expire
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(limiter.check("1.2.3.4")); // should be allowed again
    }

    #[test]
    fn test_cleanup_expired() {
        let limiter = RateLimiter::new(10, 50); // 50ms window
        limiter.check("1.2.3.4");
        limiter.check("5.6.7.8");

        // Wait for entries to expire
        std::thread::sleep(std::time::Duration::from_millis(60));
        limiter.cleanup_expired();

        // The entries should have been cleaned up, but new requests still work
        assert!(limiter.check("1.2.3.4"));
        assert!(limiter.check("5.6.7.8"));
    }

    #[test]
    fn test_extract_client_ip_forwarded() {
        let mut request = Request::new(axum::body::Body::empty());
        request
            .headers_mut()
            .insert("x-forwarded-for", "10.0.0.1, 172.16.0.1".parse().unwrap());
        let ip = extract_client_ip(&request);
        assert_eq!(ip, "10.0.0.1");
    }

    #[test]
    fn test_extract_client_ip_real_ip() {
        let mut request = Request::new(axum::body::Body::empty());
        request
            .headers_mut()
            .insert("x-real-ip", "10.0.0.2".parse().unwrap());
        let ip = extract_client_ip(&request);
        assert_eq!(ip, "10.0.0.2");
    }

    #[test]
    fn test_extract_client_ip_no_headers() {
        let request = Request::new(axum::body::Body::empty());
        let ip = extract_client_ip(&request);
        // Without ConnectInfo or headers, falls back to "unknown"
        assert_eq!(ip, "unknown");
    }

    #[test]
    fn test_rate_limiter_zero_window_uses_default() {
        let limiter = RateLimiter::new(5, 0);
        assert_eq!(limiter.window_ms, 1000);
    }
}
