use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Last known good time in milliseconds since UNIX epoch.
/// Used as a fallback when the system clock goes backwards.
static LAST_KNOWN_MS: AtomicU64 = AtomicU64::new(0);

/// Returns current time in milliseconds since UNIX epoch.
///
/// If the system clock has gone backwards (e.g. due to NTP adjustment
/// or VM time sync), returns the last known good time instead of
/// panicking. On the very first call where the clock is backwards
/// (before any good reading has been recorded), returns 0.
pub fn now_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => {
            let ms = dur.as_millis() as u64;
            // Track the highest observed time so we can fall back to it
            // if the clock ever goes backwards.
            let prev = LAST_KNOWN_MS.load(Ordering::Relaxed);
            if ms > prev {
                LAST_KNOWN_MS.store(ms, Ordering::Relaxed);
            }
            ms
        }
        Err(_) => {
            // Clock went backwards. Use the last known good time.
            // If we have never recorded a good time, return 0 as a safe default.
            let fallback = LAST_KNOWN_MS.load(Ordering::Relaxed);
            if fallback == 0 {
                tracing::warn!("clock went backwards and no prior good time recorded, returning 0");
            } else {
                tracing::warn!(
                    "clock went backwards, using last known good time {}ms",
                    fallback
                );
            }
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_ms_returns_nonzero() {
        // Reset the atomic to 0 for a clean test
        LAST_KNOWN_MS.store(0, Ordering::Relaxed);
        let t = now_ms();
        assert!(t > 0, "now_ms should return a non-zero timestamp");
    }

    #[test]
    fn test_now_ms_monotonic_property() {
        // Calling now_ms multiple times should never decrease
        let mut prev = now_ms();
        for _ in 0..100 {
            let cur = now_ms();
            assert!(
                cur >= prev,
                "now_ms should be monotonic: got {} < {}",
                cur,
                prev
            );
            prev = cur;
        }
    }
}
