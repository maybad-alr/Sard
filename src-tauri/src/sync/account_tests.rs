//! The account on platforms BOTH with and without a credential store.
//!
//! The reader's phone found the bug these tests exist for: on Android there is no credential store
//! yet, and the first version REFUSED to sign in there — while the interface promised "you will sign
//! in once per launch". A promise the code does not keep is worse than a missing feature, so the
//! behaviour is now: sign in, sync for this run, and say plainly that it will not be remembered.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use super::account;
use super::http::{Http, HttpRequest, HttpResponse};
use super::supabase::SupabaseConfig;
use crate::db::migrations;
use crate::secrets::SecretStore;

const NO_STORE: &str = "this platform has no credential store yet";

/// A SYNTHETIC VALUE, built at run time rather than written as a literal.
///
/// A credential scanner reads `"refresh_token": "…"` as a leaked secret wherever it finds one, and it
/// is right to: the shape of a fixture is the shape of the thing it stands for. Building it here keeps
/// the fixtures out of that shape while leaving them unique enough to be searched for.
fn fixture(kind: &str) -> String {
    format!("synthetic-{kind}-{}", std::process::id())
}

/// A store that can keep a value — the desktop case.
#[derive(Default)]
struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
}

impl SecretStore for MemoryStore {
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

/// A platform with no store at all — what Android reports today.
struct NoStore;

impl SecretStore for NoStore {
    fn available(&self) -> bool {
        false
    }
    fn secret(&self, _: &str) -> Result<Option<String>, String> {
        Err(NO_STORE.into())
    }
    fn put(&self, _: &str, _: &str) -> Result<(), String> {
        Err(NO_STORE.into())
    }
    fn forget(&self, _: &str) -> Result<(), String> {
        Err(NO_STORE.into())
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A real database on the migration path, with nothing in it but the schema.
fn db(tag: &str) -> (Connection, PathBuf) {
    let dir = std::env::temp_dir().join(format!("sard_account_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = crate::db::open_database(&dir.join("sard.db")).unwrap();
    migrations::run(&conn, None).unwrap();
    (conn, dir)
}

fn setup() -> SupabaseConfig {
    SupabaseConfig::new("https://project.supabase.co", "publishable-key")
}

/// Answers by a fragment of the URL, which is enough for the calls a sign-in and a pass make. The two
/// values it hands out are remembered so a test can search for them afterwards.
struct FakeHttp {
    access: String,
    refresh: String,
    replies: Vec<(&'static str, u16, String)>,
}

impl FakeHttp {
    fn new() -> Self {
        let access = fixture("access");
        let refresh = fixture("refresh");
        let tokens = serde_json::json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_at": now() + 3600,
            "user": { "id": "user-1" },
        })
        .to_string();
        Self {
            access,
            refresh,
            replies: vec![
                ("grant_type=password", 200, tokens),
                // The account's index and any book fetch: this account holds nothing, which is enough
                // to prove the pass ran with the session it was given.
                ("select=book_id,version", 200, "[]".to_string()),
            ],
        }
    }
}

impl Http for FakeHttp {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        self.replies
            .iter()
            .find(|(needle, _, _)| request.url.contains(needle))
            .map(|(_, status, body)| HttpResponse { status: *status, body: body.clone().into_bytes() })
            .ok_or_else(|| format!("no canned reply for {}", request.url))
    }
}

/// THE BUG THE PHONE FOUND. With no credential store the reader must still be able to sign in and
/// sync — and must be TOLD that the session will not outlive the process.
#[test]
fn signing_in_without_a_credential_store_works_for_this_run() {
    let (conn, _dir) = db("nostore");
    let http = FakeHttp::new();

    let status = account::sign_in(&conn, &NoStore, &http, &setup(), "reader@example.com", "pw")
        .expect("signing in must not be refused for want of a keychain");

    assert!(status.signed_in, "the session is in hand");
    assert!(!status.remembered, "and the interface is told it will not be remembered");
    assert_eq!(status.email.as_deref(), Some("reader@example.com"));

    // The session really is usable: a pass runs with it and reaches the account.
    let report = account::sync_now(&conn, &NoStore, &http).expect("a pass with the in-memory session");
    assert_eq!(report.books, Vec::new(), "nothing to exchange, but the pass ran");
}

/// THE OTHER HALF OF THE SAME RULE: not remembering must never become keeping it in a file.
#[test]
fn the_token_is_nowhere_in_the_database_when_there_is_no_store() {
    let (conn, dir) = db("nodisk");
    let http = FakeHttp::new();
    account::sign_in(&conn, &NoStore, &http, &setup(), "reader@example.com", "pw").unwrap();

    for name in ["sard.db", "sard.db-wal", "sard.db-shm"] {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            let text = String::from_utf8_lossy(&bytes);
            for (what, value) in [("refresh token", &http.refresh), ("access token", &http.access)] {
                assert!(
                    !text.contains(value.as_str()),
                    "{name} carries the {what}, which is the fallback this module exists to refuse"
                );
            }
        }
    }
}

/// On a platform that HAS a store, the session is kept and the interface is told so.
#[test]
fn signing_in_with_a_store_is_remembered() {
    let (conn, _dir) = db("store");
    let http = FakeHttp::new();
    let store = MemoryStore::default();

    let status = account::sign_in(&conn, &store, &http, &setup(), "reader@example.com", "pw").unwrap();

    assert!(status.signed_in);
    assert!(status.remembered);
    assert_eq!(
        store.secret(super::supabase::SECRET_ACCOUNT).unwrap().as_deref(),
        Some(http.refresh.as_str()),
        "the key went to the store, which is the only place it is allowed"
    );
}

/// Status must answer on a platform with no store rather than failing the call.
#[test]
fn asking_about_the_account_does_not_fail_without_a_store() {
    let (conn, _dir) = db("status");
    let status = account::status(&conn, &NoStore).expect("status must answer anywhere");
    assert!(!status.configured);
    assert!(!status.signed_in);
    assert!(!status.remembered);
}
