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
    #[error("refused to fetch {url}")]
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
        Self::build(user_agent, allow, reqwest::header::HeaderMap::new())
    }

    /// A client that sends a credential header on every request.
    ///
    /// The header rides on redirect hops too, and reqwest only strips the standard auth headers
    /// when a redirect crosses hosts. So `allow` must hold only the hosts the credential is
    /// meant for: then a redirect anywhere else is refused before the header can leave.
    pub fn with_secret_header(
        user_agent: &str,
        allow: HostAllowlist,
        name: &'static str,
        value: &str,
    ) -> Result<Self, HttpError> {
        let mut value =
            reqwest::header::HeaderValue::from_str(value).map_err(|_| HttpError::Transport {
                url: "<client>".into(),
                // Deliberately not echoing the value: it is a secret.
                message: format!("the {name} value contains characters a header cannot carry"),
            })?;
        // Keeps it out of reqwest's Debug output.
        value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(name, value);
        Self::build(user_agent, allow, headers)
    }

    fn build(
        user_agent: &str,
        allow: HostAllowlist,
        headers: reqwest::header::HeaderMap,
    ) -> Result<Self, HttpError> {
        let allow = Arc::new(allow);

        // The redirect policy is the only place a hop can be vetted before it is followed.
        let policy = {
            let allow = Arc::clone(&allow);
            reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many redirects");
                }
                let from = attempt
                    .previous()
                    .last()
                    .expect("a redirect always has a previous URL")
                    .clone();
                match allow.check_redirect(&from, attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(e),
                }
            })
        };

        let inner = reqwest::Client::builder()
            .user_agent(user_agent)
            .default_headers(headers)
            .redirect(policy)
            // Deliberately no total-request timeout. A JDK is a few hundred megabytes, and a
            // whole-request deadline turns a slow link into a hard failure no retry can fix.
            // Bound connecting and stalling instead: a transfer that is still making progress
            // is not a problem, however long it takes.
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
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

    /// A send failure that is really an allowlist refusal on a redirect hop.
    ///
    /// reqwest reports it as a generic error, which would otherwise be retried and then
    /// reported as a network problem. It is a security refusal, and retrying cannot help.
    fn refused_redirect(url: &str, e: &reqwest::Error) -> Option<HttpError> {
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
        while let Some(c) = cause {
            if let Some(host) = c.downcast_ref::<HostError>() {
                return Some(HttpError::Host {
                    url: url.to_owned(),
                    source: host.clone(),
                });
            }
            cause = c.source();
        }
        None
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
                Err(e) => {
                    if let Some(refused) = Self::refused_redirect(url, &e) {
                        return Err(refused);
                    }
                    (None, true, e.to_string())
                }
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
                    if let Some(refused) = Self::refused_redirect(url, &e) {
                        return Err(refused);
                    }
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

    /// Stream a response body into the content store.
    ///
    /// Streaming rather than buffering matters once JDKs are in scope: holding a few hundred
    /// megabytes in memory to hash it is avoidable, and on a small VPS it is the difference
    /// between working and being killed by the OOM reaper.
    async fn stream_to_store(
        &self,
        url: &str,
        expect: Option<&Digest>,
        expect_size: Option<u64>,
        store: &BlobStore,
    ) -> Result<Blob, HttpError> {
        use futures::StreamExt;

        let parsed = url::Url::parse(url).map_err(|_| HttpError::BadUrl {
            url: url.to_owned(),
        })?;
        self.allow
            .check(&parsed)
            .map_err(|source| HttpError::Host {
                url: url.to_owned(),
                source,
            })?;

        self.await_budget().await;
        let resp = self.inner.get(parsed).send().await.map_err(|e| {
            Self::refused_redirect(url, &e).unwrap_or_else(|| HttpError::Transport {
                url: url.to_owned(),
                message: e.to_string(),
            })
        })?;

        let status = resp.status().as_u16();
        self.observe(resp.headers(), status);
        if !resp.status().is_success() {
            return Err(HttpError::Status {
                url: url.to_owned(),
                status,
            });
        }

        // Pipe the body through a blocking writer so hashing never stalls the runtime.
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(8);
        let store = store.clone();
        let expect = expect.cloned();
        let writer = tokio::task::spawn_blocking(move || {
            let mut reader = ChannelReader {
                rx,
                current: Vec::new(),
                offset: 0,
            };
            store.insert_reader(&mut reader, expect.as_ref(), expect_size)
        });

        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| HttpError::Transport {
                url: url.to_owned(),
                message: e.to_string(),
            })?;
            if tx.send(Ok(chunk.to_vec())).is_err() {
                // The writer stopped early, which means it failed; its error is the real one.
                break;
            }
        }
        drop(tx);

        writer
            .await
            .map_err(|e| HttpError::Transport {
                url: url.to_owned(),
                message: e.to_string(),
            })?
            .map_err(HttpError::Blob)
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
            && let Some(blob) = store.get_verified(d)?
        {
            return Ok(blob);
        }

        let mut last: Option<HttpError> = None;
        for url in urls {
            match self.stream_to_store(url, expect, expect_size, store).await {
                Ok(blob) => return Ok(blob),
                // A bad mirror should not doom the download; try the next one.
                Err(e) => last = Some(e),
            }
        }

        Err(last.unwrap_or(HttpError::BadUrl {
            url: "<no mirrors given>".into(),
        }))
    }
}

/// Adapts the chunk channel to `Read` for the blocking hasher.
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    current: Vec<u8>,
    offset: usize,
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.offset >= self.current.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.current = chunk;
                    self.offset = 0;
                }
                Ok(Err(e)) => return Err(e),
                // Sender dropped: the body is complete.
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len() - self.offset);
        buf[..n].copy_from_slice(&self.current[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
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

    #[test]
    fn a_bad_secret_is_reported_without_echoing_it() {
        let err = HttpClient::with_secret_header(
            "hopper-test/0.1.0",
            HostAllowlist::curseforge_api(),
            "x-api-key",
            "sec\nret-value",
        )
        .unwrap_err();
        assert!(!err.to_string().contains("ret-value"), "{err}");
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
