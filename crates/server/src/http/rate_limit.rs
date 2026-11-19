//! `rate_limit`: a token bucket per client, in memory.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hyper::HeaderMap;
use hyper::header::HeaderName;
use rxh_core::config;

/// Number of buckets before the first sweep of the idle ones.
const SWEEP_AT: usize = 1024;

/// Runtime state of one `rate_limit` table.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    /// Tokens a bucket gains per second.
    refill: f64,

    /// Capacity of a bucket; a new client starts with a full one.
    burst: f64,

    /// Header whose value identifies a client, else the client IP does.
    header: Option<HeaderName>,

    /// Hashes header values, so a client cannot make the keys large.
    hasher: RandomState,

    buckets: Mutex<Buckets>,
}

#[derive(Debug)]
struct Buckets {
    map: HashMap<Key, Bucket>,

    /// Size of `map` at which full buckets are removed.
    sweep_at: usize,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Key {
    Ip(IpAddr),
    Header(u64),
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl RateLimiter {
    pub(crate) fn new(limit: &config::RateLimit) -> Self {
        let header = match &limit.key {
            config::RateLimitKey::ClientIp => None,
            config::RateLimitKey::Header(name) => Some(
                HeaderName::from_bytes(name.as_bytes())
                    .expect("the configuration parser checked the header name"),
            ),
        };
        Self {
            refill: f64::from(limit.rate) / limit.per.as_secs_f64(),
            burst: f64::from(limit.burst()),
            header,
            hasher: RandomState::new(),
            buckets: Mutex::new(Buckets {
                map: HashMap::new(),
                sweep_at: SWEEP_AT,
            }),
        }
    }

    /// Takes one token for the request of `client`. On an empty bucket,
    /// returns how long until the next token.
    pub(crate) fn check(&self, client: IpAddr, headers: &HeaderMap) -> Result<(), Duration> {
        let value = self.header.as_ref().and_then(|name| headers.get(name));
        let key = match value {
            Some(value) => Key::Header(self.hasher.hash_one(value.as_bytes())),
            None => Key::Ip(client.to_canonical()),
        };
        self.take(key, Instant::now())
    }

    fn take(&self, key: Key, now: Instant) -> Result<(), Duration> {
        let mut buckets = self
            .buckets
            .lock()
            .expect("nothing panics while holding the bucket lock");

        if buckets.map.len() >= buckets.sweep_at {
            // A full bucket is the same as no bucket.
            buckets
                .map
                .retain(|_, bucket| self.tokens(bucket, now) < self.burst);
            buckets.sweep_at = (buckets.map.len() * 2).max(SWEEP_AT);
        }

        let bucket = buckets.map.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        bucket.tokens = self.tokens(bucket, now);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / self.refill))
        }
    }

    /// Tokens of `bucket` at `now`.
    fn tokens(&self, bucket: &Bucket, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(bucket.updated);
        (bucket.tokens + elapsed.as_secs_f64() * self.refill).min(self.burst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(rate: u32, per: Duration, burst: Option<u32>) -> RateLimiter {
        RateLimiter::new(&config::RateLimit {
            rate,
            per,
            burst,
            key: config::RateLimitKey::ClientIp,
        })
    }

    fn ip(last: u32) -> Key {
        Key::Ip(IpAddr::from(last.to_be_bytes()))
    }

    #[test]
    fn bucket_empties_then_refills_at_the_rate() {
        let limiter = limiter(2, Duration::from_secs(1), Some(3));
        let start = Instant::now();

        for _ in 0..3 {
            assert_eq!(limiter.take(ip(1), start), Ok(()));
        }
        assert_eq!(
            limiter.take(ip(1), start),
            Err(Duration::from_millis(500)),
            "2 per second is one token every 500ms"
        );
        assert_eq!(limiter.take(ip(2), start), Ok(()), "one bucket per client");

        let later = start + Duration::from_millis(500);
        assert_eq!(limiter.take(ip(1), later), Ok(()));
        assert!(limiter.take(ip(1), later).is_err());

        // A long pause refills up to the burst, not beyond.
        let much_later = later + Duration::from_secs(3600);
        for _ in 0..3 {
            assert_eq!(limiter.take(ip(1), much_later), Ok(()));
        }
        assert!(limiter.take(ip(1), much_later).is_err());
    }

    #[test]
    fn per_scales_the_rate_and_burst_defaults_to_rate() {
        let limiter = limiter(2, Duration::from_secs(60), None);
        let start = Instant::now();

        assert_eq!(limiter.take(ip(1), start), Ok(()));
        assert_eq!(limiter.take(ip(1), start), Ok(()));
        assert_eq!(limiter.take(ip(1), start), Err(Duration::from_secs(30)));
    }

    #[test]
    fn header_values_get_their_own_buckets() {
        let limiter = RateLimiter::new(&config::RateLimit {
            rate: 1,
            per: Duration::from_secs(60),
            burst: None,
            key: config::RateLimitKey::Header(String::from("X-Api-Key")),
        });
        let client = IpAddr::from([10, 0, 0, 1]);
        let headers = |key: &str| -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert("x-api-key", key.parse().unwrap());
            headers
        };

        assert!(limiter.check(client, &headers("alice")).is_ok());
        assert!(limiter.check(client, &headers("alice")).is_err());
        assert!(limiter.check(client, &headers("bob")).is_ok());
        // Without the header the client IP is the key.
        assert!(limiter.check(client, &HeaderMap::new()).is_ok());
        assert!(limiter.check(client, &HeaderMap::new()).is_err());
        assert!(
            limiter
                .check(IpAddr::from([10, 0, 0, 2]), &HeaderMap::new())
                .is_ok()
        );
    }

    #[test]
    fn full_buckets_are_swept() {
        let limiter = limiter(1, Duration::from_secs(1), Some(1));
        let start = Instant::now();
        for client in 0..SWEEP_AT as u32 {
            assert_eq!(limiter.take(ip(client), start), Ok(()));
        }
        assert_eq!(limiter.buckets.lock().unwrap().map.len(), SWEEP_AT);

        // Half a second later every bucket is still refilling: all are kept.
        let later = start + Duration::from_millis(500);
        assert!(limiter.take(ip(0), later).is_err());
        let buckets = limiter.buckets.lock().unwrap();
        assert_eq!(buckets.map.len(), SWEEP_AT);
        assert_eq!(buckets.sweep_at, SWEEP_AT * 2);
        drop(buckets);

        // Once they are full again they are dropped at the next sweep.
        for client in SWEEP_AT as u32..2 * SWEEP_AT as u32 {
            assert_eq!(limiter.take(ip(client), later), Ok(()));
        }
        let much_later = later + Duration::from_secs(5);
        assert_eq!(limiter.take(ip(0), much_later), Ok(()));
        let buckets = limiter.buckets.lock().unwrap();
        assert_eq!(buckets.map.len(), 1, "only the bucket just used is left");
        assert_eq!(buckets.sweep_at, SWEEP_AT);
    }
}
