//! Test-only helpers for `device.rs`'s two-phase rotation tests (WP-B1b): a
//! mock cloud server (mirrors `dirad::test_support::MockCloud`) and a mock OS
//! keychain (mirrors `dira_core::identity::tests::use_mock_keychain`) so a
//! successful rotation's `identity::promote_pending_key` — which installs the
//! new key via the same keychain-or-meta-fallback path production uses — never
//! touches the real OS keychain during a test run.
//!
//! Also [`scripted_server`], the raw-TCP mock both of `dira update`'s network
//! hops are retry-tested against. It lives here rather than in either caller's
//! test module because the two hops now share one retry driver, and testing
//! them against two different servers would be testing two different things.

#![cfg(test)]

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One scripted response for [`scripted_server`].
#[derive(Clone)]
pub enum Reply {
    /// A complete, well-formed 200.
    Body(&'static str),
    /// A status line with no body (plus optional extra headers, each already
    /// `\r\n`-terminated).
    Status(u16, &'static str),
    /// Announce a `Content-Length` far larger than what is actually sent, then
    /// drop the connection — the exact shape of the reported failure
    /// ("connection closed before message completed"): the body starts
    /// arriving and the stream dies part-way through.
    Truncated,
    /// A 200 declaring `Content-Length: n` and sending no body at all. For
    /// asserting a size cap fires before any read is attempted.
    DeclaredLength(u64),
    /// A well-formed 200 with a body but deliberately no `Content-Length`
    /// header at all — terminated by closing the connection, the way a real
    /// server without a known length would. A size cap must never reject this:
    /// a missing declared length is not the same claim as an oversized one.
    NoContentLength(&'static str),
}

/// A raw HTTP server on an OS-assigned loopback port that serves `script` in
/// order (the last entry repeats once exhausted), counting connections so a
/// test can assert exactly how many attempts were made. Returns the base URL
/// (no trailing slash) and that counter.
///
/// Deliberately not [`MockCloud`]: axum cannot express a half-written body,
/// which is the whole point of [`Reply::Truncated`].
pub async fn scripted_server(script: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted server");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst);

            // Drain the request head so the client sees a well-formed exchange
            // rather than a connection reset on write.
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;

            let reply = script
                .get(n)
                .or_else(|| script.last())
                .cloned()
                .unwrap_or(Reply::Status(500, ""));
            match reply {
                Reply::Body(body) => {
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                }
                Reply::Status(code, extra) => {
                    let head = format!("HTTP/1.1 {code} X\r\n{extra}Content-Length: 0\r\n\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                }
                Reply::Truncated => {
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nhalf")
                        .await;
                    // Dropping `sock` here closes mid-body.
                }
                Reply::DeclaredLength(n) => {
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                }
                Reply::NoContentLength(body) => {
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    // Dropping `sock` here is how a Content-Length-less
                    // response is framed: read until EOF.
                }
            }
            let _ = sock.flush().await;
        }
    });

    (format!("http://{addr}"), hits)
}

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
