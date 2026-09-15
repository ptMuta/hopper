//! The HTTP client.
//!
//! Three things here are load-bearing and easy to get wrong elsewhere:
//!
//! * **The allowlist is enforced on every redirect hop.** Checking only the URL written in a
//!   manifest is not enough: an allowed host answering `302` with an arbitrary `Location` would
//!   bypass the list entirely. reqwest's redirect policy is where that check has to live,
//!   because by the time a response comes back the redirect has already been followed.
//! * **Downloads are verified before they are usable.** Content streams into the
//!   content-addressed store, which rejects anything whose hash does not match, so a corrupted
//!   or substituted file never becomes a cache entry a later run would trust.
//! * **Rate limits are observed from every response**, including errors — a 404 consumed quota
//!   just as a 200 did.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::ratelimit::{Clock, RateGate, RetryPolicy, SystemClock};
use crate::cache::{Blob, BlobError, BlobStore};
use crate::model::Digest;
use crate::net::{HostAllowlist, HostError};

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("refused to fetch {url}: {source}")]
    Host {
        url: String,
        #[source]
        source: HostError,
    },
    #[error("{url} returned HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("network error fetching {url}: {message}")]
    Transport { url: String, message: String },
    #[error("gave up on {url} after {attempts} attempts: {message}")]
    Exhausted {
        url: String,
        attempts: u32,
        message: String,
    },
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("{url} is not a valid URL")]
    BadUrl { url: String },
}

impl HttpError {
    /// Whether this is worth telling the operator to simply try again.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transport { .. }
                | Self::Exhausted { .. }
                | Self::Status {
                    status: 500..=599,
                    ..
                }
        )
    }
}

/// Shared HTTP client.
///
/// Cheap to clone: the underlying reqwest client pools connections, and the rate gate is shared
/// so several concurrent requests observe one budget rather than each keeping its own.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    gate: Arc<Mutex<RateGate>>,
    retry: RetryPolicy,
    clock: Arc<dyn Clock>,
    allow: Arc<HostAllowlist>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Build a client that will only ever talk to hosts on `allow`.
    pub fn new(user_agent: &str, allow: HostAllowlist) -> Result<Self, HttpError> {
        let allow = Arc::new(allow);

        // The redirect policy is the only place a hop can be vetted before it is followed.
        let policy = {
            let allow = Arc::clone(&allow);
            reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many redirects");
                }
                match allow.check(attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(e),
                }
            })
        };

        let inner = reqwest::Client::builder()
            .user_agent(user_agent)
            .redirect(policy)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| HttpError::Transport {
                url: "<client>".into(),
                message: e.to_string(),
            })?;

        Ok(Self {
            inner,
            gate: Arc::new(Mutex::new(RateGate::new())),
            retry: RetryPolicy::default(),
            clock: Arc::new(SystemClock::default()),
            allow,
        })
    }

    /// Replace the clock. Used by tests to make backoff instantaneous.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn allowlist(&self) -> &HostAllowlist {
        &self.allow
    }

    /// Wait until the rate gate says a request may go out.
    async fn await_budget(&self) {
        let delay = {
            let gate = self.gate.lock().expect("rate gate poisoned");
            gate.delay_before_next(self.clock.now_ms())
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    fn observe(&self, headers: &reqwest::header::HeaderMap, status: u16) {
        let num = |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.parse().ok() };
        let now = self.clock.now_ms();
        let mut gate = self.gate.lock().expect("rate gate poisoned");
        gate.observe(
            num("x-ratelimit-limit").map(|n| n as u32),
            num("x-ratelimit-remaining").map(|n| n as u32),
            num("x-ratelimit-reset"),
            now,
        );
        if status == 429 {
            gate.throttled(num("retry-after"), now);
        }
    }

    /// GET, with retries, rate limiting and allowlist enforcement.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let parsed = url::Url::parse(url).map_err(|_| HttpError::BadUrl {
            url: url.to_owned(),
        })?;
        self.allow
            .check(&parsed)
            .map_err(|source| HttpError::Host {
                url: url.to_owned(),
                source,
            })?;

        let mut last = String::new();
        for attempt in 0..self.retry.max_attempts {
            self.await_budget().await;

            let result = self.inner.get(parsed.clone()).send().await;
            let (status, retryable, body) = match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    self.observe(resp.headers(), status);
                    if resp.status().is_success() {
                        match resp.bytes().await {
                            Ok(b) => return Ok(b.to_vec()),
                            Err(e) => (None, true, e.to_string()),
                        }
                    } else {
                        (Some(status), false, format!("HTTP {status}"))
                    }
                }
                Err(e) => (None, true, e.to_string()),
            };
            last = body;

            let retry_status = status.filter(|_| !retryable);
            if !self.retry.should_retry(attempt, retry_status) {
                return Err(match status {
                    Some(s) => HttpError::Status {
                        url: url.to_owned(),
                        status: s,
                    },
                    None => HttpError::Exhausted {
                        url: url.to_owned(),
                        attempts: attempt + 1,
                        message: last,
                    },
                });
            }
            tokio::time::sleep(self.retry.backoff(attempt)).await;
        }

        Err(HttpError::Exhausted {
            url: url.to_owned(),
            attempts: self.retry.max_attempts,
            message: last,
        })
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<T, HttpError> {
        let bytes = self.get_bytes(url).await?;
        serde_json::from_slice(&bytes).map_err(|e| HttpError::Transport {
            url: url.to_owned(),
            message: format!("unexpected response shape: {e}"),
        })
    }

    /// POST JSON and decode the response. Used for the batch endpoints.
    pub async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, HttpError> {
        let parsed = url::Url::parse(url).map_err(|_| HttpError::BadUrl {
            url: url.to_owned(),
        })?;
        self.allow
            .check(&parsed)
            .map_err(|source| HttpError::Host {
                url: url.to_owned(),
                source,
            })?;

        let mut last = String::new();
        for attempt in 0..self.retry.max_attempts {
            self.await_budget().await;

            match self.inner.post(parsed.clone()).json(body).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    self.observe(resp.headers(), status);
                    if resp.status().is_success() {
                        return resp.json().await.map_err(|e| HttpError::Transport {
                            url: url.to_owned(),
                            message: format!("unexpected response shape: {e}"),
                        });
                    }
                    if !self.retry.should_retry(attempt, Some(status)) {
                        return Err(HttpError::Status {
                            url: url.to_owned(),
                            status,
                        });
                    }
                    last = format!("HTTP {status}");
                }
                Err(e) => {
                    last = e.to_string();
                    if !self.retry.should_retry(attempt, None) {
                        return Err(HttpError::Exhausted {
                            url: url.to_owned(),
                            attempts: attempt + 1,
                            message: last,
                        });
                    }
                }
            }
            tokio::time::sleep(self.retry.backoff(attempt)).await;
        }

        Err(HttpError::Exhausted {
            url: url.to_owned(),
            attempts: self.retry.max_attempts,
            message: last,
        })
    }

    /// Fetch into the content store, trying each mirror in turn.
    ///
    /// Verification happens inside the store, so a mismatched download is discarded rather than
    /// published — and because the server directory is untouched until afterwards, a failure
    /// here really does leave it unchanged.
    pub async fn fetch_to_store(
        &self,
        urls: &[String],
        expect: Option<&Digest>,
        expect_size: Option<u64>,
        store: &BlobStore,
    ) -> Result<Blob, HttpError> {
        // A cache hit skips the network entirely, which is what makes re-running after a
        // failure cheap and multi-server boxes share one copy.
        if let Some(d) = expect
            && d.algo() == crate::model::HashAlgo::Sha512
            && let Some(blob) = store.get(d)?
        {
            return Ok(blob);
        }

        let mut last: Option<HttpError> = None;
        for url in urls {
            match self.get_bytes(url).await {
                Ok(bytes) => match store.insert_bytes(&bytes, expect) {
                    Ok(blob) => {
                        if let Some(expected) = expect_size
                            && blob.size != expected
                        {
                            last = Some(HttpError::Blob(BlobError::SizeMismatch {
                                expected,
                                actual: blob.size,
                            }));
                            continue;
                        }
                        return Ok(blob);
                    }
                    // A bad mirror should not doom the download; try the next one.
                    Err(e) => last = Some(HttpError::Blob(e)),
                },
                Err(e) => last = Some(e),
            }
        }

        Err(last.unwrap_or(HttpError::BadUrl {
            url: "<no mirrors given>".into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> HttpClient {
        HttpClient::new("hopper-test/0.1.0", HostAllowlist::packs()).unwrap()
    }

    #[tokio::test]
    async fn refuses_a_host_that_is_not_allowlisted() {
        let err = client()
            .get_bytes("https://evil.test/payload.jar")
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Host { .. }), "got {err:?}");
        // Never reaches the network at all.
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn refuses_plaintext() {
        let err = client()
            .get_bytes("http://cdn.modrinth.com/x.jar")
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Host { .. }));
    }

    #[tokio::test]
    async fn refuses_a_malformed_url() {
        assert!(matches!(
            client().get_bytes("not a url").await.unwrap_err(),
            HttpError::BadUrl { .. }
        ));
    }

    #[tokio::test]
    async fn a_cached_blob_is_returned_without_touching_the_network() {
        // The url is deliberately unreachable: if this test passes, no request was made.
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let blob = store.insert_bytes(b"cached content", None).unwrap();

        let got = client()
            .fetch_to_store(
                &["https://cdn.modrinth.com/does-not-exist".into()],
                Some(&blob.digest),
                None,
                &store,
            )
            .await
            .unwrap();
        assert_eq!(got.digest, blob.digest);
    }

    #[tokio::test]
    async fn fetching_with_no_mirrors_fails_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        assert!(
            client()
                .fetch_to_store(&[], None, None, &store)
                .await
                .is_err()
        );
    }

    #[test]
    fn transient_failures_are_distinguished_from_permanent_ones() {
        // Drives the difference between "try again" and "this will never work".
        assert!(
            HttpError::Status {
                url: "u".into(),
                status: 503
            }
            .is_transient()
        );
        assert!(
            !HttpError::Status {
                url: "u".into(),
                status: 404
            }
            .is_transient()
        );
        assert!(
            !HttpError::Host {
                url: "u".into(),
                source: HostError::NoHost
            }
            .is_transient()
        );
    }

    #[test]
    fn the_client_carries_its_allowlist() {
        let c = HttpClient::new("hopper-test/0.1.0", HostAllowlist::runtimes()).unwrap();
        assert!(
            c.allowlist()
                .check(&url::Url::parse("https://api.adoptium.net/v3/x").unwrap())
                .is_ok()
        );
        // And the pack and runtime domains stay separate.
        assert!(
            c.allowlist()
                .check(&url::Url::parse("https://cdn.modrinth.com/x.jar").unwrap())
                .is_err()
        );
    }
}
