//! Test-only helpers for `device.rs`'s two-phase rotation tests (WP-B1b): a
//! mock cloud server (mirrors `dirad::test_support::MockCloud`) and a mock OS
//! keychain (mirrors `dira_core::identity::tests::use_mock_keychain`) so a
//! successful rotation's `identity::promote_pending_key` — which installs the
//! new key via the same keychain-or-meta-fallback path production uses — never
//! touches the real OS keychain during a test run.

#![cfg(test)]

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Once};

/// One canned HTTP response.
#[derive(Clone)]
pub struct MockResp {
    pub status: u16,
    pub body: String,
}

impl MockResp {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

type Queue = Arc<Mutex<VecDeque<MockResp>>>;

/// A minimal mock cloud bound to an OS-assigned loopback port. Each registered
/// path serves a FIFO queue of canned responses (falling back to `200 {}`
/// once drained); request bodies are recorded per-path so a test can assert
/// on exactly what was sent (e.g. that a retry reused the identical envelope).
pub struct MockCloud {
    base_url: String,
    routes: HashMap<&'static str, Queue>,
    recorded: HashMap<&'static str, Arc<Mutex<Vec<String>>>>,
}

impl MockCloud {
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
                post(move |headers: HeaderMap, body: Bytes| {
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
                        Response::builder()
                            .status(StatusCode::from_u16(resp.status).unwrap())
                            .body(axum::body::Body::from(resp.body))
                            .unwrap()
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

    pub fn push(&self, path: &'static str, resp: MockResp) {
        self.routes
            .get(path)
            .unwrap_or_else(|| panic!("mock cloud: unregistered path {path}"))
            .lock()
            .unwrap()
            .push_back(resp);
    }

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
/// default, so `identity::promote_pending_key`'s keychain write in these
/// tests never touches (or blocks on) the real OS keychain. Mirrors
/// `dira_core::identity::tests::use_mock_keychain` exactly (see its doc
/// comment for why the one-time platform-store primer + per-test reset are
/// both needed).
///
/// The mock store is process-global (a single shared table, not one per
/// `Entry`), so every test that calls this MUST also hold [`keychain_lock`]
/// for the duration of its keychain-touching work — otherwise one test's
/// reset/write can interleave with another's read and produce exactly the
/// kind of cross-test corruption `dira_core::identity`'s `ENV_LOCK` exists to
/// prevent (now that both the active AND pending device keys resolve through
/// this same keychain-first ladder, more tests than before touch it).
pub fn use_mock_keychain() {
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
