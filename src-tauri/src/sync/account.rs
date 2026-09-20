//! The account as the application holds it: what is saved, what is only in memory, and where a pass
//! starts from.
//!
//! # Three kinds of state, kept apart on purpose
//!
//!   · the SETUP (project URL, publishable key, email, user id) is ordinary `settings` rows. None of it
//!     is a secret: the publishable key ships inside every client of this service by design, and the
//!     reader's email is something they would read out to support.
//!   · the REFRESH TOKEN is the account's key, and it goes to the credential store with every other
//!     secret — never into `sard.db`, which is copied, backed up and moved between machines.
//!   · the ACCESS TOKEN is kept in memory for the length of the process and written nowhere at all.
//!     It is short-lived by design, and a copy of it on disk would be a credential with no purpose.
//!
//! # Why one process-wide cache
//!
//! One application, one window, one account: the session cache is a `static` because that is exactly
//! what it is. It is not shared state in any interesting sense — it holds one value, and the value can
//! always be rebuilt from the refresh token, which is what happens on the first pass after a launch.

use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;

use super::http::Http;
use super::supabase::{self, Session, SignUp, SupabaseBackend, SupabaseConfig};
use super::{sync_all, SyncReport};
use crate::secrets::SecretStore;

const KEY_URL: &str = "sync.url";
const KEY_ANON: &str = "sync.anon_key";
const KEY_EMAIL: &str = "sync.email";
const KEY_USER_ID: &str = "sync.user_id";

/// The access token for this process, and nothing else.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// What the interface needs to render the account without touching the network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    /// A project URL and key are saved.
    pub configured: bool,
    /// A refresh token is held, so a pass can start without signing in again.
    pub signed_in: bool,
    pub email: Option<String>,
    /// The saved project, so the form opens FILLED rather than asking a reader to find the two values
    /// again — they are not secrets, and re-typing them is the kind of small chore that makes a
    /// feature feel unfinished.
    pub url: Option<String>,
    pub key: Option<String>,
}

/// The saved setup, if there is one.
pub fn config(conn: &Connection) -> rusqlite::Result<Option<SupabaseConfig>> {
    let Some(url) = crate::settings::get(conn, KEY_URL)? else { return Ok(None) };
    let Some(anon_key) = crate::settings::get(conn, KEY_ANON)? else { return Ok(None) };
    Ok(Some(SupabaseConfig::new(url, anon_key)))
}

/// The process's HTTP client: one agent, so a pass reuses the connection it already opened and a
/// reader's second sync is cheaper than their first.
pub fn http() -> &'static dyn Http {
    static HTTP: std::sync::OnceLock<super::http::UreqHttp> = std::sync::OnceLock::new();
    HTTP.get_or_init(super::http::UreqHttp::new)
}

/// Save the setup, refusing a URL that could not be sent to.
///
/// The check is here so a reader learns about a mistyped URL while they are looking at the field, and
/// again on the way out of every request — see `SupabaseConfig::check`.
pub fn save_config(conn: &Connection, config: &SupabaseConfig) -> Result<(), String> {
    config.check()?;
    if config.anon_key.trim().is_empty() {
        return Err("sync.err.key".into());
    }
    crate::settings::set(conn, KEY_URL, config.url.trim()).map_err(|e| e.to_string())?;
    crate::settings::set(conn, KEY_ANON, config.anon_key.trim()).map_err(|e| e.to_string())
}

/// Read the account without a network call: what is saved, and whether a key is held for it.
pub fn status(conn: &Connection, secrets: &dyn SecretStore) -> Result<AccountStatus, String> {
    let setup = config(conn).map_err(|e| e.to_string())?;
    let email = crate::settings::get(conn, KEY_EMAIL).map_err(|e| e.to_string())?;
    let held = secrets.secret(supabase::SECRET_ACCOUNT).map_err(|e| e)?;
    Ok(AccountStatus {
        configured: setup.is_some(),
        signed_in: held.is_some(),
        email,
        url: setup.as_ref().map(|s| s.url.clone()),
        key: setup.as_ref().map(|s| s.anon_key.clone()),
    })
}

/// Sign in, and keep what makes the next launch a pass rather than a form.
pub fn sign_in(
    conn: &Connection,
    secrets: &dyn SecretStore,
    http: &dyn Http,
    setup: &SupabaseConfig,
    email: &str,
    password: &str,
) -> Result<AccountStatus, String> {
    save_config(conn, setup)?;
    let session = supabase::sign_in(http, setup, email, password)?;
    adopt(conn, secrets, email, session)?;
    status(conn, secrets)
}

/// Create an account. With confirmation switched on this is not an error and not yet a session — the
/// caller is told which, because "check your email" is a different sentence from "you are signed in".
pub fn sign_up(
    conn: &Connection,
    secrets: &dyn SecretStore,
    http: &dyn Http,
    setup: &SupabaseConfig,
    email: &str,
    password: &str,
) -> Result<SignUp, String> {
    save_config(conn, setup)?;
    match supabase::sign_up(http, setup, email, password)? {
        SignUp::SignedIn(session) => {
            let session = *session;
            adopt(conn, secrets, email, session.clone())?;
            Ok(SignUp::SignedIn(Box::new(session)))
        }
        SignUp::ConfirmEmail => Ok(SignUp::ConfirmEmail),
    }
}

/// Forget the account: the key goes, and with it the ability to start a pass. The project URL and key
/// stay, because a reader who signs out to sign in as someone else should not have to find them again.
pub fn sign_out(conn: &Connection, secrets: &dyn SecretStore) -> Result<(), String> {
    secrets.forget(supabase::SECRET_ACCOUNT)?;
    *SESSION.lock().unwrap() = None;
    for key in [KEY_EMAIL, KEY_USER_ID] {
        conn.execute("DELETE FROM settings WHERE key = ?1", [key]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Run one pass over the library, signing in again from the stored key if the session is gone.
pub fn sync_now(
    conn: &Connection,
    secrets: &dyn SecretStore,
    http: &dyn Http,
) -> Result<SyncReport, String> {
    let setup = config(conn).map_err(|e| e.to_string())?.ok_or_else(|| "sync.err.notConfigured".to_string())?;
    setup.check()?;
    let session = current_session(conn, secrets, http, &setup)?
        .ok_or_else(|| "sync.err.notSignedIn".to_string())?;
    let backend = SupabaseBackend::new(setup, http, secrets, session);
    sync_all(conn, &backend)
}

/// A usable session: the one in memory, or one rebuilt from the stored key.
fn current_session(
    conn: &Connection,
    secrets: &dyn SecretStore,
    http: &dyn Http,
    setup: &SupabaseConfig,
) -> Result<Option<Session>, String> {
    if let Some(session) = SESSION.lock().unwrap().clone() {
        return Ok(Some(session));
    }
    let Some(stored) = secrets.secret(supabase::SECRET_ACCOUNT)? else { return Ok(None) };
    // The stored value is the refresh token and nothing else, so rebuilding means asking for a session
    // — one round trip on the first pass after a launch, and never again in that process.
    let session = supabase::sign_in_with_refresh(http, setup, &stored)?;
    let user_id = crate::settings::get(conn, KEY_USER_ID).map_err(|e| e.to_string())?;
    let session = match user_id {
        // The user id is kept so `versions` and the row filters can name the reader without decoding
        // the token. A stored value that disagrees with the service's answer is replaced by it.
        Some(id) if id != session.user_id => {
            crate::settings::set(conn, KEY_USER_ID, &session.user_id).map_err(|e| e.to_string())?;
            session
        }
        Some(_) => session,
        None => {
            crate::settings::set(conn, KEY_USER_ID, &session.user_id).map_err(|e| e.to_string())?;
            session
        }
    };
    *SESSION.lock().unwrap() = Some(session.clone());
    Ok(Some(session))
}

/// Keep a fresh session: the key in the credential store, the rest in settings and memory.
fn adopt(
    conn: &Connection,
    secrets: &dyn SecretStore,
    email: &str,
    session: Session,
) -> Result<(), String> {
    // The secret first: if the credential store refuses, the reader gets that error rather than a
    // half-saved account that cannot start a pass.
    secrets.put(supabase::SECRET_ACCOUNT, &session.refresh_token)?;
    crate::settings::set(conn, KEY_EMAIL, email.trim()).map_err(|e| e.to_string())?;
    crate::settings::set(conn, KEY_USER_ID, &session.user_id).map_err(|e| e.to_string())?;
    *SESSION.lock().unwrap() = Some(session);
    Ok(())
}
