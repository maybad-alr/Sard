//! The account backend, tested against canned replies AND against a real HTTP conversation.
//!
//! Two kinds of test live here, and both matter:
//!
//!   · canned replies, so every branch — an expired token, a lost race, a rejected password — can be
//!     reached exactly and cheaply;
//!   · ONE test that starts a server on localhost and drives the REAL client through it, because a
//!     request-building bug (a missing header, a body sent as the wrong shape, a status read wrongly)
//!     is invisible to a fake that never parses what it is given.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::doc::BookState;
use super::http::{Http, HttpRequest, HttpResponse, Method, UreqHttp};
use super::supabase::{self, SECRET_ACCOUNT, Session, SignUp, SupabaseBackend, SupabaseConfig};
use super::{SyncBackend, SyncError};
use crate::secrets::SecretStore;

fn config() -> SupabaseConfig {
    SupabaseConfig {
        url: "https://project.supabase.co".into(),
        anon_key: "publishable-key".into(),
        allow_local: false,
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn token_body(user: &str, access: &str, refresh: &str, expires_at: i64) -> String {
    serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
        "expires_at": expires_at,
        "user": { "id": user },
    })
    .to_string()
}

fn session(access: &str, refresh: &str, expires_at: i64) -> Session {
    Session {
        user_id: "user-1".into(),
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at,
    }
}

/// A store a test can inspect.
#[derive(Default)]
struct RecordingStore {
    entries: Mutex<HashMap<String, String>>,
}

impl RecordingStore {
    fn peek(&self, account: &str) -> Option<String> {
        self.entries.lock().unwrap().get(account).cloned()
    }
}

impl SecretStore for RecordingStore {
    fn secret(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.entries.lock().unwrap().get(account).cloned())
    }
    fn put(&self, account: &str, value: &str) -> Result<(), String> {
        self.entries.lock().unwrap().insert(account.to_string(), value.to_string());
        Ok(())
    }
    fn forget(&self, account: &str) -> Result<(), String> {
        self.entries.lock().unwrap().remove(account);
        Ok(())
    }
}

/// Answers by matching a fragment of the URL, so a test says which CALL it is describing rather than
/// reproducing a query string. The first reply whose fragment appears in the URL wins, so a test that
/// cares about two similar URLs registers the more specific one first.
///
/// A reply registered with `reply_once` is consumed by the first call that uses it, which is what lets
/// a test say "the token is rejected, then accepted" — the sequence a single canned answer cannot.
#[derive(Default)]
struct RecordingHttp {
    seen: Mutex<Vec<HttpRequest>>,
    fixed: Mutex<Vec<(String, u16, String)>>,
    once: Mutex<Vec<(String, u16, String)>>,
}

impl RecordingHttp {
    fn reply(&self, needle: &str, status: u16, body: &str) {
        self.fixed.lock().unwrap().push((needle.to_string(), status, body.to_string()));
    }

    fn reply_once(&self, needle: &str, status: u16, body: &str) {
        self.once.lock().unwrap().push((needle.to_string(), status, body.to_string()));
    }

    fn calls(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn last(&self) -> HttpRequest {
        self.calls().pop().expect("a request must have been sent")
    }

    fn call_matching(&self, needle: &str) -> HttpRequest {
        self.calls()
            .into_iter()
            .find(|c| c.url.contains(needle))
            .unwrap_or_else(|| panic!("no request matched {needle}"))
    }
}

impl Http for RecordingHttp {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        self.seen.lock().unwrap().push(request.clone());

        let mut once = self.once.lock().unwrap();
        if let Some(index) = once.iter().position(|(needle, _, _)| request.url.contains(needle.as_str())) {
            let (_, status, body) = once.remove(index);
            return Ok(HttpResponse { status, body: body.into_bytes() });
        }
        drop(once);

        let fixed = self.fixed.lock().unwrap();
        if let Some((_, status, body)) = fixed.iter().find(|(needle, _, _)| request.url.contains(needle.as_str())) {
            return Ok(HttpResponse { status: *status, body: body.clone().into_bytes() });
        }
        Err(format!("no canned reply for {} {}", request.url, String::new()))
    }
}

// ---- signing in -----------------------------------------------------------------------------------

#[test]
fn signing_in_returns_the_session_and_sends_what_the_service_needs() {
    let http = RecordingHttp::default();
    http.reply("grant_type=password", 200, &token_body("user-1", "access-1", "refresh-1", now() + 3600));

    let session = supabase::sign_in(&http, &config(), "reader@example.com", "hunter2").unwrap();

    assert_eq!(session.user_id, "user-1");
    assert_eq!(session.access_token, "access-1");
    assert_eq!(session.refresh_token, "refresh-1");

    let sent = http.last();
    assert_eq!(sent.method, Method::Post);
    assert!(sent.url.ends_with("/auth/v1/token?grant_type=password"));
    assert!(
        sent.headers.iter().any(|(k, v)| k == "apikey" && v == "publishable-key"),
        "the publishable key travels on every call"
    );
    let body = String::from_utf8(sent.body.unwrap()).unwrap();
    assert!(body.contains("\"email\":\"reader@example.com\""));
}

#[test]
fn a_wrong_password_is_a_code_the_interface_can_translate() {
    let http = RecordingHttp::default();
    http.reply(
        "grant_type=password",
        400,
        r#"{"code":400,"error_code":"invalid_credentials","msg":"Invalid login credentials"}"#,
    );

    let error = supabase::sign_in(&http, &config(), "reader@example.com", "wrong").unwrap_err();
    assert_eq!(error, "sync.err.badCredentials");
}

#[test]
fn an_unconfirmed_email_is_its_own_message() {
    let http = RecordingHttp::default();
    http.reply("grant_type=password", 400, r#"{"code":400,"error_code":"email_not_confirmed"}"#);
    assert_eq!(
        supabase::sign_in(&http, &config(), "reader@example.com", "pw").unwrap_err(),
        "sync.err.emailNotConfirmed"
    );
}

/// With "confirm email" switched on the service creates the account and returns NO tokens. That is a
/// success with a different sentence, not an error, and the two are told apart by what came back.
#[test]
fn signing_up_can_mean_confirm_your_email_instead_of_signed_in() {
    let http = RecordingHttp::default();
    http.reply("signup", 200, r#"{"id":"user-1","email":"reader@example.com"}"#);
    assert_eq!(supabase::sign_up(&http, &config(), "reader@example.com", "pw").unwrap(), SignUp::ConfirmEmail);

    let http = RecordingHttp::default();
    http.reply("signup", 200, &token_body("user-1", "access-1", "refresh-1", now() + 3600));
    assert!(matches!(
        supabase::sign_up(&http, &config(), "reader@example.com", "pw").unwrap(),
        SignUp::SignedIn(_)
    ));
}

// ---- keeping the session alive ---------------------------------------------------------------------

/// A token close to expiry is renewed BEFORE the call, and the token the service hands back is kept:
/// the service may rotate the refresh token, and one that was replaced but not saved is a session the
/// reader loses at the next launch.
#[test]
fn an_expiring_session_is_renewed_before_the_call_and_the_new_secret_is_kept() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    store.put(SECRET_ACCOUNT, "refresh-1").unwrap();

    http.reply("grant_type=refresh_token", 200, &token_body("user-1", "access-2", "refresh-2", now() + 3600));
    http.reply("select=book_id,version,doc", 200, "[]");

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() - 1));
    assert_eq!(backend.fetch("abc").unwrap(), None, "the call itself succeeded");

    assert!(
        http.calls().iter().any(|c| c.url.contains("grant_type=refresh_token")),
        "the session was renewed first"
    );
    assert_eq!(store.peek(SECRET_ACCOUNT).as_deref(), Some("refresh-2"), "and the new secret was kept");

    let data = http.call_matching("rest/v1");
    assert!(
        data.headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer access-2"),
        "the data call carried the NEW token, not the stale one"
    );
}

/// A token the service rejects is renewed ONCE and the call is retried — the difference between a
/// background sync that recovers and a sign-in prompt in the reader's face.
#[test]
fn a_rejected_token_is_renewed_once_and_the_call_is_retried() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    store.put(SECRET_ACCOUNT, "refresh-1").unwrap();

    // The stale token is refused…
    http.reply_once("select=book_id,version,doc", 401, "");
    // …the renewal succeeds…
    http.reply("grant_type=refresh_token", 200, &token_body("user-1", "access-2", "refresh-2", now() + 3600));
    // …and the retry is answered.
    http.reply("select=book_id,version,doc", 200, "[]");

    let backend = SupabaseBackend::new(config(), &http, &store, session("stale", "refresh-1", now() + 3600));
    assert_eq!(backend.fetch("abc").unwrap(), None);

    let data_calls = http.calls().into_iter().filter(|c| c.url.contains("rest/v1")).count();
    assert_eq!(data_calls, 2, "the call was retried exactly once");
}

// ---- the documents ---------------------------------------------------------------------------------

#[test]
fn a_book_the_account_never_saw_is_none() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    http.reply("select=book_id,version,doc", 200, "[]");

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    assert_eq!(backend.fetch("book-hash").unwrap(), None);

    let sent = http.last();
    assert!(sent.url.contains("user_id=eq.user-1"), "the reader's own rows only: {}", sent.url);
    assert!(sent.url.contains("book_id=eq.book-hash"), "and only this book: {}", sent.url);
    assert!(sent.headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer access-1"));
}

#[test]
fn a_book_the_account_has_comes_back_with_its_version() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    let doc = BookState { chapters_read: vec![1, 2, 3], ..BookState::default() };
    http.reply(
        "select=book_id,version,doc",
        200,
        &serde_json::json!([{ "book_id": "book-hash", "version": 7, "doc": doc }]).to_string(),
    );

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    let remote = backend.fetch("book-hash").unwrap().expect("the account has this book");

    assert_eq!(remote.version, 7);
    assert_eq!(remote.doc, doc);
}

#[test]
fn creating_a_book_returns_the_first_version() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    http.reply("/rest/v1/book_state", 201, r#"[{"book_id":"book-hash","version":1}]"#);

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    let version = backend.store("book-hash", &BookState::default(), None).unwrap();

    assert_eq!(version, 1);
    let sent = http.last();
    assert_eq!(sent.method, Method::Post);
    let body = String::from_utf8(sent.body.unwrap()).unwrap();
    assert!(body.contains("\"user_id\":\"user-1\""), "the row is filed under the signed-in account");
    assert!(body.contains("\"version\":1"));
    assert!(
        sent.headers.iter().any(|(k, v)| k == "Prefer" && v.contains("return=representation")),
        "the written row is asked for, so a silent refusal cannot look like success"
    );
}

/// A create that collides with a row another device just made is the SAME race as an update that
/// arrives too late, and the merge layer above must not have to know that one is a 409 and the other
/// is an empty array.
#[test]
fn a_create_that_collides_is_a_conflict() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    http.reply(
        "/rest/v1/book_state",
        409,
        r#"{"code":"23505","message":"duplicate key value violates unique constraint"}"#,
    );

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    assert_eq!(backend.store("book-hash", &BookState::default(), None).unwrap_err(), SyncError::Conflict);
}

#[test]
fn an_update_that_won_returns_the_next_version() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    http.reply("version=eq.5", 200, r#"[{"book_id":"book-hash","version":6}]"#);

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    assert_eq!(backend.store("book-hash", &BookState::default(), Some(5)).unwrap(), 6);

    let sent = http.last();
    assert_eq!(sent.method, Method::Patch);
    assert!(sent.url.contains("version=eq.5"), "the write is conditional on the version we read");
    let body = String::from_utf8(sent.body.unwrap()).unwrap();
    assert!(body.contains("\"version\":6"), "and it writes the next one");
}

/// THE RACE. The service answers a conditional update that matched nothing with an EMPTY ARRAY and a
/// 200 — not an error status — so the body is what says the write lost.
#[test]
fn an_update_the_account_has_moved_past_is_a_conflict() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    http.reply("version=eq.5", 200, "[]");

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    assert_eq!(
        backend.store("book-hash", &BookState::default(), Some(5)).unwrap_err(),
        SyncError::Conflict,
        "an empty result is a lost race, not a success"
    );
}

#[test]
fn versions_lists_the_accounts_index() {
    let http = RecordingHttp::default();
    let store = RecordingStore::default();
    http.reply(
        "select=book_id,version",
        200,
        r#"[{"book_id":"b","version":2},{"book_id":"a","version":1}]"#,
    );

    let backend = SupabaseBackend::new(config(), &http, &store, session("access-1", "refresh-1", now() + 3600));
    assert_eq!(
        backend.versions().unwrap(),
        vec![("b".to_string(), 2), ("a".to_string(), 1)]
    );
}

// ---- where a request may be sent --------------------------------------------------------------------

/// THE URL RULE. The project URL is typed in by the reader and every request carries an access token,
/// so a value that points inside their own machine or network sends that token somewhere it does not
/// belong — and a link-local address reaches the metadata service a cloud host runs.
#[test]
fn a_pasted_url_may_not_point_inside_the_readers_own_network() {
    let refused = [
        ("ftp://project.supabase.co", "sync.err.urlScheme"),
        ("javascript:alert(1)", "sync.err.urlScheme"),
        ("", "sync.err.urlScheme"),
        ("https://", "sync.err.urlShape"),
        ("https://user:secret@project.supabase.co", "sync.err.urlShape"),
        ("http://localhost:8080", "sync.err.urlLocal"),
        ("http://localhost./", "sync.err.urlLocal"),
        ("http://something.localhost/", "sync.err.urlLocal"),
        ("http://127.0.0.1", "sync.err.urlLocal"),
        ("http://127.1.2.3:5432/rest", "sync.err.urlLocal"),
        ("http://[::1]:8080", "sync.err.urlLocal"),
        ("http://[::ffff:127.0.0.1]", "sync.err.urlLocal"),
        ("http://10.0.0.5", "sync.err.urlLocal"),
        ("http://172.16.4.4", "sync.err.urlLocal"),
        ("http://192.168.1.1", "sync.err.urlLocal"),
        ("http://169.254.169.254/latest/meta-data/", "sync.err.urlLocal"),
        ("http://100.100.0.1", "sync.err.urlLocal"),
        ("http://0.0.0.0", "sync.err.urlLocal"),
        ("http://255.255.255.255", "sync.err.urlLocal"),
        ("http://224.0.0.1", "sync.err.urlLocal"),
        ("http://[fe80::1]", "sync.err.urlLocal"),
        ("http://[fd00::1]", "sync.err.urlLocal"),
    ];
    for (url, expected) in refused {
        assert_eq!(
            supabase::check_host(url, false).unwrap_err(),
            expected,
            "{url} must be refused"
        );
    }

    let accepted = [
        // A stand-in shape, never a real project: the reference belongs to whoever runs one, and a
        // repository is not the place to publish it.
        "https://project-ref.supabase.co",
        "https://project.supabase.co:443/rest/v1",
        "http://example.com",
        "https://8.8.8.8",
        "https://[2606:4700::1111]",
    ];
    for url in accepted {
        assert!(supabase::check_host(url, false).is_ok(), "{url} must be allowed");
    }
}

/// The escape that the transport test uses is a TEST-ONLY door, and it opens onto loopback only: a
/// private network or a metadata address is refused even with it.
#[test]
fn the_test_only_local_escape_still_refuses_a_private_network() {
    assert!(supabase::check_host("http://127.0.0.1:1234", true).is_ok());
    assert_eq!(supabase::check_host("http://192.168.1.1", true).unwrap_err(), "sync.err.urlLocal");
    assert_eq!(
        supabase::check_host("http://169.254.169.254", true).unwrap_err(),
        "sync.err.urlLocal"
    );
}

/// The rule runs on the way OUT, not only when the value was saved.
#[test]
fn a_request_to_a_local_host_never_leaves_the_process() {
    let http = RecordingHttp::default();
    let local = SupabaseConfig {
        url: "http://127.0.0.1:9999".into(),
        anon_key: "publishable-key".into(),
        allow_local: false,
    };

    let error = supabase::sign_in(&http, &local, "reader@example.com", "pw").unwrap_err();
    assert_eq!(error, "sync.err.urlLocal");
    assert!(http.calls().is_empty(), "no request was sent at all");
}

// ---- the real client, over a real socket ------------------------------------------------------------

/// A server that answers canned replies, matched by a fragment of the request target, one connection
/// at a time. It exists to run the REAL transport at least once: a fake never parses what it is given,
/// so a request built wrongly would look fine to every test above.
struct FakeServer {
    port: u16,
}

impl FakeServer {
    fn start(replies: Vec<(&'static str, u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let target: String;
                let mut length = 0usize;
                {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    let _method = parts.next();
                    target = parts.next().unwrap_or_default().to_string();
                    for header in reader.by_ref().lines() {
                        let Ok(header) = header else { break };
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                // Drain the body so the client's write completes before the reply.
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);

                let (_, status, payload) = replies
                    .iter()
                    .find(|(needle, _, _)| target.contains(needle))
                    .expect("the server must have a reply for this request");
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { port }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[test]
fn the_real_client_speaks_http_to_a_real_server() {
    let server = FakeServer::start(vec![
        (
            "grant_type=password",
            200,
            token_body("user-1", "access-1", "refresh-1", now() + 3600),
        ),
        ("select=book_id,version,doc", 200, r#"[{"book_id":"b","version":3,"doc":{"format":1,"chaptersRead":[4]}}]"#.to_string()),
    ]);
    let http = UreqHttp::new();
    let store = RecordingStore::default();
    // `allow_local` exists for this test alone — the rule it bypasses is checked below, on its own.
    let api = SupabaseConfig { url: server.url(), anon_key: "publishable-key".into(), allow_local: true };

    let session = supabase::sign_in(&http, &api, "reader@example.com", "pw").expect("sign-in over real HTTP");
    let backend = SupabaseBackend::new(api, &http, &store, session);
    let remote = backend.fetch("book-hash").expect("fetch over real HTTP").expect("a document");

    assert_eq!(remote.version, 3);
    assert_eq!(remote.doc.chapters_read, vec![4]);
}
