//! Test-only mock cloud server, shared by `sync`/`heartbeat`/`billing`'s unit
//! tests so each can drive its single-shot function (`flush`/`beat`/
//! `fetch_once`) against scripted HTTP responses without a real network.
//!
//! Each registered path serves a FIFO queue of canned responses; once a
//! queue drains it falls back to a generic `200 {}` (harmless for the rare
//! extra call a background task might make beyond what a test explicitly
//! scripted).

#![cfg(test)]

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One canned HTTP response.
#[derive(Clone)]
pub struct MockResp {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
    /// Sleep this long before answering — lets a test exercise a caller's
    /// own request timeout without a real network stall (repo-visibility's
    /// probe tests use this for its 2s-in-prod/short-in-test probe timeout).
    pub delay: Option<Duration>,
}

impl MockResp {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            headers: vec![],
            delay: None,
        }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            headers: vec![],
            delay: None,
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Answer this response only after `delay` — for exercising a caller's
    /// request timeout.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

type Queue = Arc<Mutex<VecDeque<MockResp>>>;

/// A minimal mock cloud bound to an OS-assigned loopback port. Recorded
/// request bodies are kept per-path so a test can assert on what was sent
/// (e.g. that a retry reused the identical rotation envelope).
pub struct MockCloud {
    base_url: String,
    routes: HashMap<&'static str, Queue>,
    recorded: HashMap<&'static str, Arc<Mutex<Vec<String>>>>,
}

impl MockCloud {
    /// Start the mock server with a handler for each of `paths` that answers
    /// any HTTP method (`POST` for the cloud-sync channels' bodies; `GET` for
    /// repo-visibility's probe requests) — the canned-response queue and
    /// recorded-bodies list don't care which method arrived.
    pub async fn start(paths: &[&'static str]) -> Self {
        let mut routes = HashMap::new();
        let mut recorded = HashMap::new();
        let mut router = Router::new();
        for &path in paths {
            let q: Queue = Arc::new(Mutex::new(VecDeque::new()));
            let rec: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            routes.insert(path, q.clone());
            recorded.insert(path, rec.clone());
            router = router.route(
                path,
                any(move |headers: HeaderMap, body: Bytes| {
                    let q = q.clone();
                    let rec = rec.clone();
                    async move {
                        let _ = &headers;
                        rec.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&body).to_string());
                        let resp = q
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| MockResp::ok("{}"));
                        if let Some(d) = resp.delay {
                            tokio::time::sleep(d).await;
                        }
                        let mut builder =
                            Response::builder().status(StatusCode::from_u16(resp.status).unwrap());
                        for (k, v) in &resp.headers {
                            builder = builder.header(k.as_str(), v.as_str());
                        }
                        builder.body(axum::body::Body::from(resp.body)).unwrap()
                    }
                }),
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock cloud");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base_url: format!("http://{addr}"),
            routes,
            recorded,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Queue one response to be served on the NEXT request to `path`.
    pub fn push(&self, path: &'static str, resp: MockResp) {
        self.routes
            .get(path)
            .unwrap_or_else(|| panic!("mock cloud: unregistered path {path}"))
            .lock()
            .unwrap()
            .push_back(resp);
    }

    /// All request bodies received on `path` so far, in order.
    pub fn requests(&self, path: &'static str) -> Vec<String> {
        self.recorded
            .get(path)
            .unwrap_or_else(|| panic!("mock cloud: unregistered path {path}"))
            .lock()
            .unwrap()
            .clone()
    }
}

/// Install a fresh, empty keyring-core mock store as the process-global
/// default, so `dira_core::identity`'s keychain-first pending/active key
/// resolution never touches (or blocks on) the real OS keychain in these
/// tests. Mirrors `dira_core::identity::tests::use_mock_keychain` /
/// `dira::test_support::use_mock_keychain` exactly (see either's doc comment
/// for why the one-time platform-store primer + per-test reset are both
/// needed).
///
/// The mock store is process-global, so every test that calls this MUST also
/// hold [`keychain_lock`] for the duration of its keychain-touching work.
pub fn use_mock_keychain() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = keyring::Entry::store_status();
    });
    keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
}

/// Serializes every test that touches the shared mock keychain (see
/// [`use_mock_keychain`]'s doc comment). Acquire this BEFORE calling
/// `use_mock_keychain()` and hold the guard for the rest of the test.
static KEYCHAIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire [`KEYCHAIN_LOCK`]. A tokio mutex (not `std`) so the guard can be
/// held across `.await` points without tripping `await_holding_lock`, and it
/// never poisons on a failed assert in another test.
pub async fn keychain_lock() -> tokio::sync::MutexGuard<'static, ()> {
    KEYCHAIN_LOCK.lock().await
}
